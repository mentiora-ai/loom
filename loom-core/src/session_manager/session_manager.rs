// SessionManager — session lifecycle FSM owner.
//
// # Contract semantics
// - **FSM:** `Created → Active → Closed | Aborted | Killed | Crashed`.
//   Transitions guarded by per-session `Mutex<SessionStatus>`.
// - **Per-session tokio task.** Each session runs on a
//   dedicated multi-threaded tokio task with its own arena.
// - **Abort propagation.** Each session owns
//   `Arc<Notify>` + `Arc<AtomicBool>`. `abort()` flips the bool and
//   calls `notify.notify_one()`. Host-fn entries check the bool;
//   `tokio::select!` races real call vs notify.
// - **Warm create budget.** ULID gen + ensure_dir + WAL
//   header append + fsync + task spawn. NO WASM/Chromium/network on the
//   sync path.
// - **Kill-callback registration.** `create()` registers an
//   `Arc<dyn Fn(SessionId, KillReason)>` into `BudgetEnforcer` so a
//   budget breach can interrupt this session's task without a
//   structural cycle.

use loom_core::budget_enforcer::{BudgetEnforcer, BudgetLimits, KillReason, SessionCounters};
use loom_core::content_store::ContentStore;
use loom_core::determinism_harness::{DeterminismHarness, TapeWriter};
use loom_core::error::LoomError;
use loom_core::manifest_writer::{ManifestWriter, SessionId, WriterHandle};
use loom_core::observability::Observability;
use loom_core::session_scope::SessionScope;
use loom_core::vault::Vault;
use loom_shared::types::{EpochMs, Seed};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Active,
    Closed,
    Aborted,
    Killed,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortReason {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreateOpts {
    pub agent_id: String,
    pub surface: String,
    pub seed: Option<u64>,
    pub limits: Option<BudgetLimits>,
    pub replay_of: Option<SessionId>,
    /// When set (replay path, OR `--clock-anchor`), forces the manifest
    /// Header's `started_at_ms` to this exact value instead of `now_ms()`,
    /// and pins `epoch_ms` (→ CDP `initialVirtualTime`, i.e. the injected
    /// `Date.now`/`performance.now`). Required for hash-chain bit-equality:
    /// the chain hashes over the canonical Header bytes, so a divergent
    /// timestamp poisons every subsequent prev_hash. `--clock-anchor` reuses
    /// this field so a fresh anchored recording reproduces the same epoch
    /// across runs (cross-run determinism) with no new opts/Header field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms_override: Option<u64>,
    /// Operator-supplied `--capture-policy`.
    /// Wire form `"minimal" | "default" | "full"`; `None` means the
    /// server-default profile applies. Persisted in the manifest Header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_policy: Option<String>,
    /// Operator's `--no-blocklist` opt-out.
    /// Default `false` (the analytics/ads/telemetry blocklist enforces
    /// sub-resource gating on every navigate). Persisted on per-session
    /// state so the host's `navigate_execute` can compute
    /// `blocklist_enabled = !no_blocklist` for each ShimRequest.
    #[serde(default)]
    pub no_blocklist: bool,
    /// Operator's `--no-determinism` opt-out (settle-capture slice 4b).
    /// Default `false` (determinism ON: the injected template freezes
    /// `Date.now`/`performance.now` and seeds `Math.random`, so captures are
    /// byte-reproducible). When `true`, the shim injects a PASS-THROUGH script
    /// instead (real wall-clock + unseeded RNG) for live/non-reproducible
    /// capture. Recorded in the manifest Header so replay REFUSES the session —
    /// a non-deterministic run can never be replay-equal. The R3 ordering gate
    /// is unchanged (inject still runs + flips the readiness flag).
    #[serde(default)]
    pub no_determinism: bool,
    /// video-capture: operator's `session create --record-screencast` opt-in.
    /// Default `false`. When `true`, the host starts a CDP screencast on the
    /// session's active page target at create time and finalizes it (encode →
    /// CAS) at session close/reset, writing the resulting hash to the per-session
    /// `recordings.jsonl` sidecar (surfaced by `loom.session.info`). The webm
    /// bytes are non-deterministic and live OUTSIDE the manifest hash chain, so
    /// this never affects replay-equality (NFR-DET-01). Opt-in mirrors the
    /// screenshot privacy posture: recordings capture whatever is on screen.
    #[serde(default)]
    pub record_screencast: bool,
    /// Operator's `--profile` choice — `"safe" | "standard" | "full"`. Wire-default
    /// is `"safe"` (per `loom_rpc::core_service_adapter::CreateSessionParams::default_profile`).
    /// Daemon's evaluate gate and shim's download confinement
    /// both branch on this value.
    #[serde(default = "default_profile_string")]
    pub profile: String,
    /// voice-call-io: operator's `session create --audio` opt-in. Default
    /// `false`. When `true`, the session is a browser voice call: the shim
    /// launches Chromium with the fake-media flags, grants the `audioCapture`
    /// permission (scoped to the navigated origin, reset on close), and installs
    /// the synthetic-microphone bootstrap (`audio_bootstrap.js`) at target spawn
    /// so `getUserMedia({audio})` resolves to a loom-controlled stream. Strictly
    /// opt-in — the override replaces `getUserMedia`, so it must never touch an
    /// ordinary session. Carried to the shim as `audio_enabled` on
    /// `SpawnTarget`/`PageNavigate` (mirrors `determinism_enabled`).
    #[serde(default)]
    pub audio: bool,
}

fn default_profile_string() -> String {
    "safe".to_string()
}

impl Default for SessionCreateOpts {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            surface: String::new(),
            seed: None,
            limits: None,
            replay_of: None,
            started_at_ms_override: None,
            capture_policy: None,
            no_blocklist: false,
            no_determinism: false,
            record_screencast: false,
            profile: default_profile_string(),
            audio: false,
        }
    }
}

/// Internal session lifecycle error. Mapped to `LoomError` at the facade
/// boundary via `From<SessionError> for LoomError`.
#[derive(Debug)]
pub enum SessionError {
    SessionUnknown {
        session_id: String,
    },
    SessionAlreadyClosed {
        session_id: String,
    },
    SessionAborted {
        reason: String,
    },
    SessionKilled {
        reason: String,
    },
    /// Profile fields are immutable after creation.
    SessionProfileImmutable,
    SessionClosed {
        closed_at_ms: u64,
    },
    ManifestError(LoomError),
}

impl From<SessionError> for LoomError {
    fn from(e: SessionError) -> LoomError {
        use loom_core::error::LoomErrorCode;
        match e {
            SessionError::SessionUnknown { session_id } => LoomError::new(
                LoomErrorCode::SessionNotFound,
                format!("session not found: {session_id}"),
            ),
            SessionError::SessionAlreadyClosed { session_id } => LoomError::new(
                LoomErrorCode::SessionAlreadyClosed,
                format!("session already closed: {session_id}"),
            ),
            SessionError::SessionAborted { reason } => LoomError::new(
                LoomErrorCode::SessionAborted,
                format!("session aborted: {reason}"),
            ),
            SessionError::SessionKilled { reason } => LoomError::new(
                LoomErrorCode::SessionKilled,
                format!("session killed: {reason}"),
            ),
            SessionError::SessionProfileImmutable => LoomError::new(
                LoomErrorCode::InvalidArgument,
                "session profile is immutable after creation",
            ),
            SessionError::SessionClosed { closed_at_ms } => LoomError::new(
                LoomErrorCode::SessionAlreadyClosed,
                format!("session closed at {closed_at_ms}"),
            ),
            SessionError::ManifestError(e) => e,
        }
    }
}

/// Activity snapshot of a single session for the reaper's idle/zombie decisions.
/// A plain data carrier so `loom-core` need not depend on the daemon's reaper types.
#[derive(Debug, Clone)]
pub struct SessionActivity {
    pub id: SessionId,
    pub last_activity_ms: u64,
    pub in_flight: u32,
    pub is_active: bool,
}

/// Per-session in-memory state. Owned via `Arc` by the session table.
pub struct Session {
    pub id: SessionId,
    /// Guarded by a sync mutex — transitions never cross await points.
    pub status: parking_lot::Mutex<SessionStatus>,
    pub abort_flag: Arc<AtomicBool>,
    pub abort_notify: Arc<Notify>,
    pub writer: Arc<WriterHandle>,
    pub counters: Arc<SessionCounters>,
    pub tape_writer: Arc<tokio::sync::Mutex<TapeWriter>>,
    /// Structured-concurrency parent for every session-lifetime task.
    /// Owns the wall-clock budget timer, shim-IPC tasks, receipt-marshaller
    /// spawns. On close/abort, the session manager calls `scope.cancel()`
    /// (sync cooperative signal) and the daemon bridge awaits
    /// `scope.drain(grace)` before returning, so the daemon never leaks
    /// fire-and-forget tasks across sessions.
    pub scope: Arc<SessionScope>,
    /// Unix epoch milliseconds of the session's last action activity, used by the
    /// idle-TTL reaper. Initialised to the session's `epoch_ms` at create and bumped
    /// at action START via `touch()` (so a session running a single long action is not
    /// seen as idle). In-memory only.
    pub last_activity_ms: AtomicU64,
    /// Count of actions currently executing for this session. The idle reaper never
    /// evicts a session with `in_flight_actions > 0` (no mid-action eviction); the close
    /// path re-checks it under the status lock. In-memory only.
    pub in_flight_actions: AtomicU32,
    /// Per-session dispatch fence (connection-protocol redesign). The daemon's
    /// `WasmBridge::dispatch_action_blocking` holds this for the FULL duration of a
    /// surface-verb dispatch — including after the RPC layer has abandoned the request
    /// on timeout/cancel (the blocking work runs detached to completion). A later
    /// action that finds the slot held fails fast with a typed `too_many_requests`
    /// instead of interleaving with the abandoned work, so a late result can never
    /// corrupt a newer request's session state and per-session WAL/receipt order
    /// stays strictly dispatch-ordered (NFR-DET-01). In-memory only.
    pub dispatch_slot: parking_lot::Mutex<()>,
    /// Per-session monotonic action sequence, 0-based. Incremented atomically
    /// at action dispatch via `allocate_action_id()`.
    /// In-memory only — NOT persisted across daemon restarts (the daemon
    /// today doesn't resume Active sessions across restarts; sessions become
    /// Crashed on restart per startup_manager). If a future release adds
    /// session-resume, rebuild this from the last ActionReceipt.action_id+1.
    pub next_action_id: AtomicU64,
    /// Budget kill metadata. Written by the kill-callback BEFORE the
    /// abort_flag is flipped + abort_notify is fired. The session
    /// executor's `tokio::select!` abort-arm reads this to distinguish
    /// budget-driven kills (→ `ActionOutcome::Trapped { BudgetExceeded }`)
    /// from user-initiated aborts (→ `ActionOutcome::Aborted`).
    pub kill_reason: Arc<parking_lot::Mutex<Option<KillReason>>>,
    /// Capture-policy wire-form ("minimal" / "default" / "full"), as
    /// passed in `SessionCreateOpts` and persisted to the manifest
    /// header. The daemon reads this at dispatch time
    /// to apply per-receipt capture-policy via
    /// `loom_rpc::host_service_adapter::wire_capture::apply_capture_profile_to_wire`.
    /// `None` means default profile.
    pub capture_policy: Option<String>,
    /// Operator's `--no-blocklist` opt-out.
    /// `false` by default (blocklist enforced). Read by the host's
    /// `navigate_execute` to compute the per-PageNavigate
    /// `blocklist_enabled` field.
    pub no_blocklist: bool,
    /// Operator's `--no-determinism` opt-out (settle-capture slice 4b).
    /// `false` by default (determinism ON). Read by the host's target-spawn
    /// path to compute the per-request `determinism_enabled = !no_determinism`
    /// so the shim injects the freeze template (true) or a pass-through (false).
    pub no_determinism: bool,
    /// video-capture: operator's `session create --record-screencast` opt-in.
    /// `false` by default. Read by the daemon to auto-start a whole-session
    /// screencast after the first navigate and finalize it at close.
    pub record_screencast: bool,
    /// voice-call-io: operator's `session create --audio` opt-in. `false` by
    /// default. Read by the host's target-spawn / navigate path to compute the
    /// per-request `audio_enabled` carried on `SpawnTarget`/`PageNavigate`, which
    /// gates the fake-media launch flags, the origin-scoped `audioCapture` grant,
    /// and the synthetic-microphone bootstrap install. Mirrors `no_determinism`.
    pub audio: bool,
    /// Per-session determinism seed. The `Option<u64> → Seed` collapse
    /// happens exactly once, at `LocalSessionManager::create` —
    /// `opts.seed.unwrap_or(default_seed)`. Downstream layers (HostState,
    /// shim wire, target_manager, determinism_injector, JS template)
    /// carry concrete `Seed(u64)` only.
    pub seed: Seed,
    /// Per-session determinism harness (virtual clock + ChaCha20 RNG),
    /// seeded with the RESOLVED `seed` above. The same resolved value is
    /// recorded in the manifest Header, and replay re-creates its session
    /// from that Header seed — so the harness seed is authoritative and
    /// identical on record and replay. Sessions created without an
    /// explicit `--seed` get a harness seeded with the facade's
    /// `default_seed` (each such session starts a FRESH stream from that
    /// seed — deterministic, and isolated from every other session).
    /// Threaded into `HostState.determinism` per dispatch, so the
    /// `rng_next_u64`/`clock_now` host fns never interleave draws across
    /// concurrent sessions.
    pub determinism: Arc<DeterminismHarness>,
    /// Per-session Unix epoch milliseconds. Substituted into the shim
    /// JS template's `Date.now` constant. Defaults to `now_ms()` at
    /// session create when `opts.started_at_ms_override` is not set,
    /// so two sessions with the same seed but different create-times
    /// get different `Date.now()` outputs (which is correct — the seed
    /// only determines RNG, not clock).
    pub epoch_ms: EpochMs,
    /// Operator's `--profile` choice persisted at create. Read by daemon's
    /// WasmBridge for the evaluate gate AND by
    /// host_function_table when spawning the per-session shim (downloads
    /// dir env var injection). Immutable after create.
    pub profile: String,
    /// Session-scoped downloads directory (`~/.loom/sessions/<ulid>/downloads/`).
    /// Populated only when `profile == "safe"` — Chromium uses this as the
    /// `downloadPath` for `Browser.setDownloadBehavior(allowAndName)` so all
    /// downloads stay confined to the session dir.
    /// `None` for non-safe profiles or pre-fix sessions.
    pub downloads_dir: Option<std::path::PathBuf>,
}

impl Session {
    /// Allocate the next monotonic action_id for this session.
    /// Returns the value the action should carry (0-based); the counter
    /// is then advanced to N+1. Relaxed ordering is sufficient: per-session
    /// dispatch is serialized today by the daemon's WasmBridge, and
    /// `fetch_add` is atomic regardless of ordering.
    pub fn allocate_action_id(&self) -> u64 {
        self.next_action_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Record activity at epoch-ms `now`, resetting the idle clock.
    pub fn touch(&self, now_ms: u64) {
        self.last_activity_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Mark an action as starting: bump the in-flight counter and reset the idle clock.
    /// Pairs with `action_finished`. Keeps a session running one long action from looking idle.
    pub fn action_started(&self, now_ms: u64) {
        self.in_flight_actions.fetch_add(1, Ordering::SeqCst);
        self.touch(now_ms);
    }

    /// Mark an action as finished: decrement the in-flight counter (saturating at 0) and
    /// reset the idle clock so the idle window starts from completion.
    pub fn action_finished(&self, now_ms: u64) {
        // A compare-exchange loop floors the counter at 0 so an unbalanced finish
        // can't underflow. Written out rather than via `fetch_update`, which newer
        // toolchains deprecate in favour of `try_update` — a method that postdates
        // our MSRV, so neither spelling compiles everywhere we support.
        let mut current = self.in_flight_actions.load(Ordering::SeqCst);
        while current > 0 {
            match self.in_flight_actions.compare_exchange_weak(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.touch(now_ms);
    }

    /// Current in-flight action count.
    pub fn in_flight(&self) -> u32 {
        self.in_flight_actions.load(Ordering::SeqCst)
    }

    /// Last activity epoch-ms.
    pub fn last_activity(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }
}

/// How many terminal (closed/aborted/killed) sessions the in-memory table
/// retains before the oldest are evicted. See the eviction-policy note on
/// `LocalSessionManager::sessions`.
pub const TERMINAL_RETENTION_CAP: usize = 128;

/// Concrete SessionManager implementation.
pub struct LocalSessionManager {
    /// In-memory session table.
    ///
    /// # Eviction policy (bounded terminal retention)
    ///
    /// Sessions are inserted at `create()` and stay in the table while
    /// Active. On every transition to a terminal state (`close`/`abort`/
    /// budget-kill) the id is recorded in `terminal_retention`, a FIFO
    /// capped at [`TERMINAL_RETENTION_CAP`]; when the FIFO overflows, the
    /// OLDEST terminal session is removed from this map. Rationale:
    ///
    /// - **Bounded memory.** Before this policy the map had exactly one
    ///   insert and zero removes, so every `Arc<Session>` (manifest paths,
    ///   counters, tape writer, scope) lived for the daemon's lifetime —
    ///   unbounded growth proportional to total sessions ever created.
    /// - **Recent terminal sessions keep their typed errors.** The daemon's
    ///   dispatch path and `close()`/`abort()` look terminal sessions up
    ///   here to return `SessionClosed`/`SessionAlreadyClosed`/
    ///   `BudgetExceeded` instead of `SessionNotFound`; retaining the most
    ///   recent CAP terminal sessions preserves that behaviour for the
    ///   window in which callers realistically race a close.
    /// - **Eviction degrades to the restart contract.** An evicted terminal
    ///   session answers `SessionNotFound` from `get()` — exactly what the
    ///   same lookup returns after a daemon restart (this table was never
    ///   persisted). Historical queries (`session.list`, `session.inspect`)
    ///   already read the on-disk manifests and are unaffected.
    /// - **Terminal sessions stop shielding GC.** Evicted ids drop out of
    ///   `live_session_ids()`, so the reaper's orphan-browser GC and
    ///   `session.reap`'s corrupt-WAL quarantine can reclaim their
    ///   leftovers instead of skipping them forever.
    ///
    /// The FSM is one-way (terminal states never revert to Active), so a
    /// FIFO pop can remove its entry unconditionally — the id can never
    /// belong to a live session again (ULIDs are never reused).
    pub(crate) sessions: dashmap::DashMap<SessionId, Arc<Session>>,
    /// FIFO of sessions that reached a terminal state, oldest first.
    /// Bounded at [`TERMINAL_RETENTION_CAP`]; overflow evicts from
    /// `sessions`. Guarded by a sync mutex — pushes never cross await
    /// points. Each session is pushed at most once: every terminal
    /// transition happens under the session's status lock and only fires
    /// from `Active`/`Created`.
    pub(crate) terminal_retention: parking_lot::Mutex<std::collections::VecDeque<SessionId>>,
    pub(crate) content_store: Arc<dyn ContentStore>,
    pub(crate) manifest_writer: Arc<dyn ManifestWriter>,
    pub(crate) vault: Arc<dyn Vault>,
    pub(crate) budget_enforcer: Arc<dyn BudgetEnforcer>,
    pub(crate) obs: Arc<Observability>,
    /// Fallback seed for sessions created without an explicit
    /// `SessionCreateOpts.seed`. Read once from the daemon config at
    /// startup. The `Option<u64> → Seed` collapse for `opts.seed.is_none()`
    /// happens exactly once, in `LocalSessionManager::create`.
    pub(crate) default_seed: u64,
    /// Sessions root directory (`<data_root>/sessions/`). Used by `create()`
    /// to build the per-session downloads directory under safe profile
    /// (`<sessions_root>/<ulid>/downloads/`).
    pub(crate) sessions_root: std::path::PathBuf,
    /// Weak self-pointer set at `new()` via `Arc::new_cyclic`. Used by
    /// `create()` to build the budget kill-callback closure without
    /// taking `Arc<Self>` as a receiver (interface_tests assert
    /// `create(&self)`). The closure upgrades the Weak on every fire to
    /// avoid a static dep cycle (BudgetEnforcer → SessionManager → ...).
    pub(crate) me: Weak<Self>,
}

impl LocalSessionManager {
    // No facade-level DeterminismHarness dependency: each session mints
    // its own harness at `create()` (seeded with the session's resolved
    // seed) so concurrent sessions never share RNG/clock state.
    pub fn new(
        content_store: Arc<dyn ContentStore>,
        manifest_writer: Arc<dyn ManifestWriter>,
        vault: Arc<dyn Vault>,
        budget_enforcer: Arc<dyn BudgetEnforcer>,
        obs: Arc<Observability>,
        default_seed: u64,
        sessions_root: std::path::PathBuf,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            sessions: dashmap::DashMap::new(),
            terminal_retention: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            content_store,
            manifest_writer,
            vault,
            budget_enforcer,
            obs,
            default_seed,
            sessions_root,
            me: me.clone(),
        })
    }

    // create(), get(), close(), abort(), abort_all() — implemented in impl_local.rs

    /// Record that `id` reached a terminal state and evict the oldest
    /// terminal sessions beyond [`TERMINAL_RETENTION_CAP`] from the
    /// in-memory table. Called from every Active→terminal transition
    /// (close/abort/budget-kill). See the eviction-policy note on
    /// `sessions`.
    pub(crate) fn note_terminal(&self, id: SessionId) {
        let mut fifo = self.terminal_retention.lock();
        fifo.push_back(id);
        while fifo.len() > TERMINAL_RETENTION_CAP {
            if let Some(victim) = fifo.pop_front() {
                // One-way FSM: the victim is guaranteed still-terminal, so
                // removal is unconditional. Disk remains the source of
                // truth for historical queries.
                self.sessions.remove(&victim);
            }
        }
    }

    /// Internal helper: build the kill-callback closure registered with
    /// `BudgetEnforcer`. The closure interrupts the session's tokio task
    /// via abort_notify + abort_flag without creating a static dep cycle.
    ///
    /// Idempotent — the BudgetEnforcer's per-session `killed: AtomicBool`
    /// already gates against double-fire, but the closure also tolerates
    /// being invoked twice on the same session (e.g. by separate budget
    /// kinds): `kill_reason` is only set on first fire and the status
    /// transition only happens from `Active`/`Created` (later transitions
    /// no-op).
    pub fn kill_callback_for(
        &self,
        _id: SessionId,
    ) -> Arc<dyn Fn(SessionId, KillReason) + Send + Sync> {
        let weak = self.me.clone();
        Arc::new(move |sid, reason| {
            let manager = match weak.upgrade() {
                Some(m) => m,
                None => return,
            };
            let session = match manager.sessions.get(&sid) {
                Some(s) => Arc::clone(s.value()),
                None => return,
            };

            // Capture a kind-tag for the manifest entry before move.
            let kind_tag: &'static str = match &reason {
                KillReason::BudgetExceeded { kind, .. } => match kind {
                    loom_core::budget_enforcer::ResourceKind::Walltime => "wall_clock",
                    loom_core::budget_enforcer::ResourceKind::Network => "network",
                    loom_core::budget_enforcer::ResourceKind::DomNodes => "dom_nodes",
                    loom_core::budget_enforcer::ResourceKind::JsHeap => "js_heap",
                },
                KillReason::UserAbort => "user_abort",
                KillReason::StoreFailure => "store_failure",
            };

            // Write kill_reason BEFORE flipping abort_flag — the
            // executor's abort arm reads kill_reason to disambiguate
            // budget-kill from user-abort.
            {
                let mut slot = session.kill_reason.lock();
                if slot.is_some() {
                    // Idempotent: a second kill (e.g. wall-clock + network
                    // racing) doesn't overwrite the first reason. The
                    // first kill always wins.
                    return;
                }
                *slot = Some(reason);
            }

            session.abort_flag.store(true, Ordering::Release);
            session.abort_notify.notify_one();

            // Idempotent status transition: only flip if currently Active
            // or Created. Already-terminal sessions stay terminal.
            let transitioned = {
                let mut status = session.status.lock();
                if matches!(*status, SessionStatus::Active | SessionStatus::Created) {
                    *status = SessionStatus::Killed;
                    true
                } else {
                    false
                }
            };
            // Bounded terminal retention: only the transition that actually
            // flipped the status records the session (a kill racing an
            // abort/close must not double-push the FIFO).
            if transitioned {
                manager.note_terminal(sid.clone());
            }

            // Append a SessionTerminal manifest entry tagged with the
            // budget kind. Best-effort: a write failure here can't undo
            // the kill so we ignore the error (caller already aborted).
            let _ = manager.manifest_writer.append(
                sid,
                loom_core::manifest_writer::ManifestEntry::SessionTerminal {
                    action_id: 0,
                    emitted_at_ms: now_ms(),
                    reason: format!("budget_exceeded:{kind_tag}"),
                    prev_hash: String::new(),
                },
            );
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
