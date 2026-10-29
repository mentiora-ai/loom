// ShimManager — out-of-process shim lifecycles.
//
// # Contract semantics
// - **The only direct out-of-process actor in `loom-host`.** All
//   chromium / ax interactions go through this module via
//   length-prefixed CBOR over `socketpair` for IPC framing.
// - **Pooled connections.** One `socketpair` per ShimId; the host
//   namespaces ShimId as `format!("{name}:{session_id}")` so each
//   session gets its own subprocess (reuse falls out naturally — a
//   second navigate hits the cached `ShimProcess`).
// - **Circuit breaker.** 3 consecutive failures opens the breaker for
//   5 s (`breaker_open_ms`). Subsequent `send` calls fail-fast with
//   `ShimBreakerOpen` until the open window expires; the first call
//   after expiry transitions the breaker to `HalfOpen` and proceeds as
//   a recovery probe — success closes the breaker, failure re-opens it
//   with a fresh window. Failures are classified: transport failures
//   (spawn / IO / timeout — the subprocess or socket is unhealthy) also
//   evict the live subprocess; application failures (shim-REPORTED
//   errors from a live shim, e.g. a CDP protocol error) only count
//   toward the threshold and never kill a healthy Chromium.
// - **Spawn-retry budget.** Single retry on initial spawn failure.
// - **No platform symbols here.** The shim binaries (chromium) contain
//   the platform-specific code; `ShimManager` only spawns and speaks
//   length-prefixed CBOR over the inherited socket FD.
//
// # Module layout
// This file holds the `ShimManager` struct, its construction / registration
// lifecycle, the generic `send` + `get_or_spawn` spawn path, the circuit
// breaker, session shutdown, and the health/diagnostics snapshots. The split
// siblings carry the rest (all behavior-preserving):
//   - `types`   — ids, config, breaker/failure enums, state + snapshot, the
//                 typed verb outcomes, and the per-verb `send_*` params structs.
//   - `helpers` — free-fn helpers (wall-clock, profile-dir cleanup, shim code /
//                 class mapping, CBOR extraction + evaluate-payload parsing).
//   - `senders` — the typed per-verb `send_*` methods (a second `impl` block).
// Everything public is re-exported below so `loom_host::shim_manager::{...}`
// and the in-crate `crate::shim_manager::{...}` / `super::shim_manager::{...}`
// paths resolve unchanged.

use crate::host_observability::HostObservability;
use crate::shim_manager::process::{send_and_await, shutdown_process, ShimProcess, SpawnConfig};
use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::shim_protocol::{
    ciborium_from_slice, ciborium_to_vec, CdpMessage, ShimHealthInfo, ShimRequest, ShimResponse,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

// Re-export the split-module public + crate-visible items so existing paths
// (`loom_host::shim_manager::X`, `crate::shim_manager::X`,
// `super::shim_manager::X`) resolve exactly as before the split. The globs
// also bring every helper / type into local scope for the impl below, so no
// separate private `use` is needed (a private `use` of a name the glob
// re-exports would shadow the public re-export — rustc errors on that).
pub use super::helpers::*;
pub use super::types::*;

/// The manager.
pub struct ShimManager {
    pub(crate) configs: dashmap::DashMap<ShimId, ShimConfig>,
    pub(crate) states: dashmap::DashMap<ShimId, ShimState>,
    /// Live subprocess pool. One entry per `ShimId` that has a running
    /// child. Populated by lazy spawn on first `send`; drained by
    /// `shutdown_session` and on transport-failure evictions.
    pub(crate) processes: dashmap::DashMap<ShimId, Arc<ShimProcess>>,
    /// Per-id spawn mutual exclusion. Without this, two concurrent first
    /// calls for the same `ShimId` both miss the `processes` cache and
    /// both spawn — the second insert displaces the first Arc, leaving a
    /// transient untracked shim sharing the same `--user-data-dir`
    /// (split-brain + spurious first-action errors). The lock makes
    /// check-spawn-insert atomic per id; losers of the race await the
    /// winner and get the same `Arc<ShimProcess>`.
    pub(crate) spawn_locks: dashmap::DashMap<ShimId, Arc<tokio::sync::Mutex<()>>>,
    pub(crate) obs: Arc<HostObservability>,
    /// Allocates a stable u64 per host-side ULID for the wire's
    /// `session_id` field. Atomic counter avoids the hash-collision
    /// surface of a sha256/FxHash. Starts at 1 so 0 retains its
    /// "no real session" sentinel meaning.
    pub(crate) host_session_ids: dashmap::DashMap<String, u64>,
    pub(crate) host_session_counter: AtomicU64,
    /// Background cleanup tasks evicted from `processes` by
    /// `record_failure` (transport-failure eviction). The previous code
    /// used a fire-and-forget `tokio::spawn` here, which leaked
    /// `JoinHandle`s — when `shutdown_process` hung on SIGTERM grace
    /// each eviction left a never-joined task behind, and after a few
    /// sessions the daemon's runtime saturated. Now the spawn is owned
    /// by this `JoinSet` and reaped opportunistically on every
    /// `record_failure` and on every `shutdown_session`.
    pub(crate) cleanup_tasks: parking_lot::Mutex<JoinSet<()>>,
}

impl ShimManager {
    pub fn new(obs: Arc<HostObservability>) -> Arc<Self> {
        Arc::new(Self {
            configs: dashmap::DashMap::new(),
            states: dashmap::DashMap::new(),
            processes: dashmap::DashMap::new(),
            spawn_locks: dashmap::DashMap::new(),
            obs,
            host_session_ids: dashmap::DashMap::new(),
            host_session_counter: AtomicU64::new(0),
            cleanup_tasks: parking_lot::Mutex::new(JoinSet::new()),
        })
    }

    /// Map a host ULID to a stable u64 for the shim wire. Idempotent —
    /// calling twice with the same ULID returns the same u64. Threadsafe.
    pub fn shim_session_id_for(&self, ulid: &str) -> u64 {
        if let Some(v) = self.host_session_ids.get(ulid) {
            return *v;
        }
        let new = self.host_session_counter.fetch_add(1, Ordering::SeqCst) + 1;
        *self.host_session_ids.entry(ulid.to_string()).or_insert(new)
    }

    /// Register a shim's spawn config. Idempotent — replaces any
    /// existing entry for `id`.
    pub fn register(&self, id: ShimId, config: ShimConfig) {
        self.configs.insert(id, config);
    }

    /// Whether `id` has been registered.
    pub fn is_registered(&self, id: &ShimId) -> bool {
        self.configs.contains_key(id)
    }

    /// Send `msg` (opaque CBOR-encoded `CdpMessage` from the WASM guest)
    /// to `id` and await the response. Honors send/recv timeouts and
    /// the circuit breaker.
    pub async fn send(&self, id: ShimId, msg: Vec<u8>) -> Result<Vec<u8>, LoomError> {
        self.check_breaker(&id)?;

        // Decode the opaque CBOR bytes into a `CdpMessage`. The WASM
        // guest's cdp_message_encoder produces this shape. A decode
        // failure here is the HOST's problem (bad guest payload) — the
        // shim did nothing wrong, so it counts as an application failure
        // and must not kill the subprocess.
        let cdp_msg: CdpMessage = match ciborium_from_slice(&msg) {
            Ok(m) => m,
            Err(e) => {
                self.record_failure(&id, FailureClass::Application);
                return Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: opaque payload not a CdpMessage: {e}", id.0),
                ));
            }
        };

        // Look up config.
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;

        let process = self.get_or_spawn(&id, &config).await?;

        let request = ShimRequest::CdpSend {
            request_id: 0, // overwritten by send_and_await
            session_id: 0,
            target_id: 0,
            message: cdp_msg,
        };

        // animation-capture (Mode B, D11): cap-align the generic CdpSend recv
        // timeout with the RPC request-timeout so a heavy standalone screenshot
        // (which rides this leg) isn't cut short while the RPC layer allowed more.
        let recv_ms = effective_generic_recv_ms(config.recv_timeout_ms, rpc_request_timeout_ms());
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("re-encode response: {e}"),
                    ));
                }
                Ok(bytes)
            }
            Ok(ShimResponse::Error { code, detail, .. }) => {
                self.record_failure(&id, shim_error_class(&code));
                Err(LoomError::new(
                    map_shim_code(code),
                    format!("shim {}: {}", id.0, detail),
                ))
            }
            Ok(other) => {
                // CdpEvent / LogLine on the response oneshot is a protocol
                // violation — those go through the demux task's separate
                // path. If we got one here, the demux logic is buggy.
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: unexpected non-Ok response: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, host_call_class(&e));
                Err(e)
            }
        }
    }

    pub(crate) async fn get_or_spawn(
        &self,
        id: &ShimId,
        config: &ShimConfig,
    ) -> Result<Arc<ShimProcess>, LoomError> {
        // Fast path: hand back a cached, live shim without touching the
        // spawn lock.
        if let Some(p) = self.processes.get(id) {
            if !p.crashed.load(Ordering::SeqCst) {
                return Ok(p.clone());
            }
        }

        // Slow path: per-id mutual exclusion around check-spawn-insert.
        // Clone the Arc out of the entry guard before awaiting so no
        // DashMap shard lock is held across the `.await`.
        let spawn_lock = self
            .spawn_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = spawn_lock.lock().await;

        // Re-check under the lock: a concurrent caller may have spawned
        // while we waited. Proactive liveness check: only hand back a
        // cached shim if it is still alive. A crashed chromium/CDP shim
        // that's still in the map would otherwise be returned as a dead
        // handle, so every session created after a browser crash inherits
        // the dead browser (the reported "sessions after a crash get a
        // dead browser" bug). Evict the corpse and fall through to
        // respawn. The breaker (checked by callers before reaching here)
        // bounds repeated crash-respawn churn.
        let cached_dead = {
            if let Some(p) = self.processes.get(id) {
                if !p.crashed.load(Ordering::SeqCst) {
                    return Ok(p.clone());
                }
                true
            } else {
                false
            }
        };
        if cached_dead {
            // Drop the dead handle (its watcher already reaped the OS process);
            // remove() is a no-op if a concurrent caller beat us to it.
            self.processes.remove(id);
            tracing::warn!(shim = %id.0, "evicting crashed shim before reuse; respawning");
        }
        let spawn_config = SpawnConfig {
            binary_path: config.binary_path.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
        };
        // Pre-spawn snapshot: did a state entry already exist? If yes the
        // upcoming spawn is a respawn, not a first spawn. Gating on this
        // avoids overcounting in the open-breaker path (record_failure
        // creates the state entry but the next call would otherwise be
        // rejected by the breaker before reaching here).
        let is_respawn = self.states.contains_key(id);
        match crate::shim_manager::process::spawn_shim(&spawn_config).await {
            Ok(p) => {
                self.processes.insert(id.clone(), p.clone());
                if is_respawn {
                    // Bump restart bookkeeping; create the entry first if
                    // somehow missing (record_failure should have created it
                    // but guard for the corner case).
                    let mut s = self.states.entry(id.clone()).or_insert_with(|| ShimState {
                        id: id.clone(),
                        breaker: BreakerState::Closed,
                        consecutive_failures: 0,
                        opened_at_ms: None,
                        restart_count: 0,
                        last_restart_at_ms: None,
                    });
                    s.restart_count = s.restart_count.saturating_add(1);
                    s.last_restart_at_ms = Some(now_ms());
                }
                Ok(p)
            }
            Err(e) => {
                self.record_failure(id, FailureClass::Transport);
                Err(e)
            }
        }
    }

    /// Cooperatively shut down all shim subprocesses bound to
    /// `session_id` (matched on the `:<session_id>` suffix of the
    /// ShimId). Called by the daemon's session-close handler.
    pub async fn shutdown_session(&self, session_id: &str) {
        let suffix = format!(":{session_id}");
        let keys: Vec<ShimId> = self
            .processes
            .iter()
            .filter(|kv| kv.key().0.ends_with(&suffix))
            .map(|kv| kv.key().clone())
            .collect();
        for key in keys {
            if let Some((_, process)) = self.processes.remove(&key) {
                shutdown_process(process).await;
            }
            self.states.remove(&key);
            self.configs.remove(&key);
            self.spawn_locks.remove(&key);
        }
        // Drop the ULID -> wire-id mapping for this session. `host_session_ids`
        // is keyed by the raw session ULID (not the `:<sid>`-suffixed ShimId),
        // so it is removed here independently of the process/state/config maps.
        // Without this the map grows monotonically under session churn — a
        // small but unbounded leak, with stale mappings persisting forever
        // (audit 2026-06-10).
        self.host_session_ids.remove(session_id);
        // Remove the per-session chromium profile dir
        // (`<tmp>/loom-chromium-<session_id>`). Without this it leaks forever
        // and accumulates across a long-running daemon (a contributor to the
        // gradual degradation) — and leaves the session's cookies/state on disk.
        // Done AFTER shutdown_process so chromium has released `--user-data-dir`;
        // NotFound is fine (idempotent / already gone via crash reap). Offloaded
        // to a blocking thread: a recursive `remove_dir_all` of a populated
        // chromium profile can do real disk I/O and must not stall a Tokio
        // worker.
        let sid = session_id.to_string();
        let _ = tokio::task::spawn_blocking(move || remove_session_profile_dir(&sid)).await;
        // Reap any completed breaker-eviction cleanup tasks. Lock release
        // happens at scope exit; JoinSet::try_join_next is non-blocking.
        let mut set = self.cleanup_tasks.lock();
        while set.try_join_next().is_some() {}
    }

    /// Gate a send path on the circuit breaker.
    ///
    /// `Closed` (or untracked) passes. `Open` fail-fasts with
    /// `ShimBreakerOpen` until the open window (`breaker_open_ms`,
    /// default 5 s) has elapsed since `opened_at_ms`; the first call
    /// after expiry transitions the breaker to `HalfOpen` and proceeds
    /// as a recovery probe. `HalfOpen` admits calls as probes (no
    /// single-probe gating — an early-return path that skipped both
    /// `record_*` calls would otherwise wedge the breaker half-open
    /// forever): the first probe success closes the breaker
    /// (`record_success`), the first probe failure re-opens it with a
    /// fresh window (`record_failure`).
    pub(crate) fn check_breaker(&self, id: &ShimId) -> Result<(), LoomError> {
        let Some(mut state) = self.states.get_mut(id) else {
            return Ok(());
        };
        match state.breaker {
            BreakerState::Closed | BreakerState::HalfOpen => Ok(()),
            BreakerState::Open => {
                let open_ms = self
                    .configs
                    .get(id)
                    .map(|c| c.breaker_open_ms)
                    .unwrap_or(5_000);
                let expired = state
                    .opened_at_ms
                    .is_none_or(|t| now_ms().saturating_sub(t) >= open_ms);
                if expired {
                    state.breaker = BreakerState::HalfOpen;
                    Ok(())
                } else {
                    Err(LoomError::new(
                        LoomErrorCode::ShimBreakerOpen,
                        format!("shim {} circuit breaker is open", id.0),
                    ))
                }
            }
        }
    }

    /// Increment the breaker counter on a failure. Opens the breaker at
    /// threshold (or immediately on a failed `HalfOpen` probe, with a
    /// fresh window either way). `Transport` failures additionally evict
    /// the live process so the next admitted call triggers a fresh
    /// spawn; `Application` failures keep the healthy subprocess (and
    /// its mid-session browser state) alive — see `FailureClass`.
    pub(crate) fn record_failure(&self, id: &ShimId, class: FailureClass) {
        let threshold = self
            .configs
            .get(id)
            .map(|c| c.breaker_threshold)
            .unwrap_or(3);
        let mut state = self.states.entry(id.clone()).or_insert_with(|| ShimState {
            id: id.clone(),
            breaker: BreakerState::Closed,
            consecutive_failures: 0,
            opened_at_ms: None,
            restart_count: 0,
            last_restart_at_ms: None,
        });
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.breaker == BreakerState::HalfOpen || state.consecutive_failures >= threshold {
            state.breaker = BreakerState::Open;
            state.opened_at_ms = Some(now_ms());
        }
        drop(state);
        // Evict the live process on transport failures only — the
        // subprocess/socket is unhealthy and the next admitted call must
        // respawn. Application failures came from a live, responsive
        // shim; evicting would kill a healthy Chromium and destroy its
        // mid-session browser state.
        if class == FailureClass::Transport {
            if let Some((_, p)) = self.processes.remove(id) {
                let mut set = self.cleanup_tasks.lock();
                set.spawn(shutdown_process(p));
                // Opportunistically reap completed cleanups so the JoinSet
                // doesn't grow unbounded across many breaker evictions.
                while set.try_join_next().is_some() {}
            }
        }
    }

    /// Reset the breaker to `Closed`. Also the `HalfOpen` → `Closed`
    /// transition: a successful recovery probe lands here.
    pub(crate) fn record_success(&self, id: &ShimId) {
        if let Some(mut s) = self.states.get_mut(id) {
            s.consecutive_failures = 0;
            s.breaker = BreakerState::Closed;
            s.opened_at_ms = None;
        }
    }

    /// Snapshot the breaker state for diagnostics.
    /// Snapshot of every tracked shim's breaker state. Used by
    /// `daemon.health` to surface per-shim circuit-breaker visibility.
    /// Cheap iteration over the DashMap; no locks held across awaits.
    pub fn breaker_state_snapshot(&self) -> Vec<(ShimId, BreakerState, u8, Option<u64>)> {
        self.states
            .iter()
            .map(|kv| {
                let s = kv.value();
                (
                    s.id.clone(),
                    s.breaker,
                    s.consecutive_failures,
                    s.opened_at_ms,
                )
            })
            .collect()
    }

    pub fn breaker_state(&self, id: &ShimId) -> Option<BreakerState> {
        self.states.get(id).map(|s| s.breaker)
    }

    /// Clone the full `ShimState` for `id`, if tracked. Used by
    /// `daemon.health({deep:true})` to surface restart bookkeeping per shim.
    pub fn shim_state(&self, id: &ShimId) -> Option<ShimStateSnapshot> {
        self.states.get(id).map(|s| ShimStateSnapshot {
            id: s.id.clone(),
            breaker: s.breaker,
            consecutive_failures: s.consecutive_failures,
            opened_at_ms: s.opened_at_ms,
            restart_count: s.restart_count,
            last_restart_at_ms: s.last_restart_at_ms,
        })
    }

    /// Snapshot every tracked shim id. Used by `daemon.health({deep:true})`
    /// to iterate the probe fan-out. Iterates `processes` (live subprocesses)
    /// rather than `states` (which can have stale entries for evicted
    /// shims) so probes only target running children.
    pub fn list_shim_ids(&self) -> Vec<ShimId> {
        self.processes.iter().map(|kv| kv.key().clone()).collect()
    }

    /// Probe a live shim for its self-reported `ShimHealthInfo`. Used by
    /// `daemon.health({deep:true})`. Does NOT lazy-spawn — if the shim is
    /// not running, returns `LoomErrorCode::ShimFailure`.
    ///
    /// Timeout: `LOOM_PROBE_TIMEOUT_MS` env override (default 1000). The
    /// timeout is enforced INSIDE `send_and_await`, so cancelling the
    /// returned future cannot leak entries from `process.pending`.
    pub async fn probe_health(&self, id: &ShimId) -> Result<ShimHealthInfo, LoomError> {
        let process = self.processes.get(id).map(|p| p.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} is not running — cannot probe", id.0),
            )
        })?;
        let recv_ms = std::env::var("LOOM_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1_000);
        // request_id is overwritten by send_and_await via set_request_id.
        let req = ShimRequest::Health { request_id: 0 };
        match send_and_await(
            &process,
            req,
            Duration::from_millis(500),
            Duration::from_millis(recv_ms),
        )
        .await?
        {
            ShimResponse::Ok { payload, .. } => {
                // Re-encode the ciborium Value and decode as ShimHealthInfo.
                let bytes = ciborium_to_vec(&payload).map_err(|e| {
                    LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("re-encode ShimHealthInfo payload: {e}"),
                    )
                })?;
                ciborium_from_slice::<ShimHealthInfo>(&bytes).map_err(|e| {
                    LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("decode ShimHealthInfo: {e}"),
                    )
                })
            }
            ShimResponse::Error { code, detail, .. } => Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim health probe returned error {code:?}: {detail}"),
            )),
            other => Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim health probe: unexpected response: {other:?}"),
            )),
        }
    }

    /// Force-close the breaker (test seam + operator recovery).
    pub fn breaker_reset(&self, id: &ShimId) {
        if let Some(mut s) = self.states.get_mut(id) {
            s.breaker = BreakerState::Closed;
            s.consecutive_failures = 0;
            s.opened_at_ms = None;
        }
    }

    /// Soft-default open window. Used by tests + diagnostics.
    pub fn breaker_open_window(&self, id: &ShimId) -> Duration {
        self.configs
            .get(id)
            .map(|c| Duration::from_millis(c.breaker_open_ms))
            .unwrap_or(Duration::from_secs(5))
    }
}
