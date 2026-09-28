// ShimManager — shared types.
//
// Plain data types used across the shim-manager submodules: ids, config,
// breaker / failure classification, per-shim live + snapshot state, the typed
// verb outcomes (`EvaluateOutcome` / `SetInputFilesOutcome`), and the per-verb
// `send_*` params structs. Split out of `shim_manager.rs` (behavior-preserving).
//
// All items are re-exported from `shim_manager.rs` (`pub use super::types::*`)
// so existing paths — `loom_host::shim_manager::{...}` and the in-crate
// `crate::shim_manager::{...}` / `super::shim_manager::{...}` — resolve
// unchanged.

use loom_shared::types::{EpochMs, Seed};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Logical shim id. Production keys are `format!("{name}:{session_id}")`
/// (e.g. `"chromium:01HXYZ..."`). Tests may use bare names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ShimId(pub String);

/// Spawn config per shim. Loaded from `HostConfig` once at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShimConfig {
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    /// Environment variables to set on the child. The daemon populates
    /// `LOOM_SHIM_CHROMIUM_PATH` and `LOOM_SHIM_USER_DATA_DIR` here;
    /// `LOOM_SHIM_FD` is set automatically at spawn time.
    pub env: Vec<(String, String)>,
    pub spawn_retry: u8,       // soft default 1
    pub breaker_threshold: u8, // soft default 3
    pub breaker_open_ms: u64,  // soft default 5000
    pub send_timeout_ms: u64,
    pub recv_timeout_ms: u64,
}

impl Default for ShimConfig {
    fn default() -> Self {
        Self {
            binary_path: PathBuf::from("/usr/local/bin/loom-shim-stub"),
            args: vec![],
            env: vec![],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 5_000,
            recv_timeout_ms: 30_000,
        }
    }
}

/// animation-capture (Mode B, D11): process-level RPC request-timeout cap
/// (`LOOM_REQUEST_TIMEOUT_MS`, mirrors `connection_handler::request_timeout`).
/// A non-positive / unparseable value falls back to the 30s default.
pub(crate) const RPC_REQUEST_TIMEOUT_ENV: &str = "LOOM_REQUEST_TIMEOUT_MS";
pub(crate) fn rpc_request_timeout_ms() -> u64 {
    std::env::var(RPC_REQUEST_TIMEOUT_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(30_000)
}

/// animation-capture (Mode B, D11): effective recv timeout for the generic
/// `CdpSend` leg = the larger of the shim's configured recv timeout and the RPC
/// request-timeout cap. The standalone `web.screenshot` rides this generic leg,
/// so without cap-alignment a heavy full-page PNG is cut at a flat 30s shim recv
/// even when the operator raised `LOOM_REQUEST_TIMEOUT_MS` (and the RPC layer
/// allowed more). Stateless (no per-call state → race-free under concurrent
/// dispatch) and WIT-free; per-call `deadline_ms` still TIGHTENS at the RPC layer.
/// Pure for unit-testability.
pub(crate) fn effective_generic_recv_ms(recv_timeout_ms: u64, rpc_cap_ms: u64) -> u64 {
    recv_timeout_ms.max(rpc_cap_ms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// Failure classification for `record_failure`.
///
/// - `Transport`: the subprocess or its socket is unhealthy (spawn
///   failure, channel closed, send/recv timeout, crash, framing/demux
///   protocol violation). Counts toward the breaker AND evicts the live
///   process so the next admitted call respawns fresh.
/// - `Application`: a live, responsive shim REPORTED an error (e.g.
///   `CdpProtocolError` / `TargetUnknown` from a bad `Runtime.evaluate`),
///   or the host failed to decode the guest's payload before the shim
///   was ever involved. Counts toward the breaker threshold but does NOT
///   evict — killing the subprocess here would destroy a healthy
///   Chromium's mid-session browser state (current page, in-memory
///   state, in-flight navigation) over an error the browser survived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    Transport,
    Application,
}

/// Per-shim live state. Tracked by `ShimManager`.
pub struct ShimState {
    pub id: ShimId,
    pub breaker: BreakerState,
    pub consecutive_failures: u8,
    pub opened_at_ms: Option<u64>,
    /// Lifecycle counters. Lives on `ShimState` (not `ShimProcess`) because
    /// `ShimProcess` is replaced on every respawn; these counters persist
    /// across the replacement so the operator can see "this shim has been
    /// restarted N times" via `daemon.health({deep:true})`.
    pub restart_count: u32,
    pub last_restart_at_ms: Option<u64>,
}

/// Owned snapshot of `ShimState` for callers that need to read it outside
/// the DashMap guard (e.g. async aggregators that can't hold a Ref across
/// `.await`). Mirror of the live struct.
#[derive(Debug, Clone)]
pub struct ShimStateSnapshot {
    pub id: ShimId,
    pub breaker: BreakerState,
    pub consecutive_failures: u8,
    pub opened_at_ms: Option<u64>,
    pub restart_count: u32,
    pub last_restart_at_ms: Option<u64>,
}

/// Outcome of a `set_input_files` CDP sequence. `SelectorNotFound` /
/// `NotAFileInput` are application outcomes (the host maps them to typed
/// wire `kind` strings), distinct from transport `Err(LoomError)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetInputFilesOutcome {
    Ok { file_count: u32 },
    SelectorNotFound,
    NotAFileInput,
}

/// Outcome of a trusted-input CDP sequence (`send_type_keystrokes` /
/// `send_press_key` / `send_trusted_click`). Application outcomes the host maps
/// to typed receipt `kind` strings, distinct from transport `Err(LoomError)`:
/// - `SelectorNotFound` — a selector was given but matched no node.
/// - `NotHittable` — the element resolved but has no box model (display:none /
///   detached / zero-size), so a trusted click point can't be computed.
/// - `UnknownKey` — `web.press_key` got an unknown key name or modifier.
/// - `DispatchedAckPending` — the element resolved and the trusted input's
///   COMMIT event (`mousePressed` for click; the key/text frame for type/
///   press_key) was written to the shim, but its CDP ack never returned within
///   the dispatch budget. This is the cross-origin renderer-process-SWAP case:
///   the committing input triggered a top-level navigation that tore the stale
///   CDP session down, so the ack lands on a renderer the host isn't watching.
///   The input WAS performed (best-effort) — the daemon treats this like `Ok`
///   (same constant `outcome_hash` marker, replay-equal) and proceeds to the
///   bounded post-action settle, rather than surfacing a transport error.
///   Distinct from `Err(LoomError)`, which means the frame never left the host.
/// - `MalformedValue` — `web.type` fill set a date/time-family input by value
///   and the input rejected or normalised it (the read-back differs).
/// - `NotEditable` — `web.type` fill targeted a disabled or readonly field.
/// - `FillFailed` — `web.type` fill could not fill the element for a reason on
///   the PAGE's side (see [`FillFailure`]). Typed, so it never counts against the
///   shim's circuit breaker the way a transport failure does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDispatchOutcome {
    Ok,
    SelectorNotFound,
    NotHittable,
    UnknownKey,
    DispatchedAckPending,
    MalformedValue(SetValueType),
    NotEditable,
    FillFailed(FillFailure),
}

/// Why `web.type` fill could not fill the element it resolved. Each maps to a
/// FIXED message: page-authored text (an exception message, an odd verdict, a
/// CDP error about the page's node) never reaches a receipt — the agentic test
/// studio hands receipt errors to an LLM agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillFailure {
    /// The page replaced or removed the element after it was focused.
    Detached,
    /// `DOM.resolveNode` handed back no object for the element.
    NoObject,
    /// Chromium refused to resolve or run on the element (e.g. it is gone).
    Rejected,
    /// The page threw while the field was being prepared.
    PageException,
    /// The prepare step answered with something other than a known verdict.
    UnrecognisedVerdict,
}

impl FillFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::Detached => {
                "web.type fill: the element was detached from the document before it could be filled"
            }
            Self::NoObject => "web.type fill: DOM.resolveNode returned no object for the element",
            Self::Rejected => "web.type fill: the browser could not reach the element to fill it",
            Self::PageException => "web.type fill: the page threw while the field was being prepared",
            Self::UnrecognisedVerdict => {
                "web.type fill: unrecognised result from the field-prepare step"
            }
        }
    }
}

/// An `<input>` type that `web.type` fill sets by VALUE (native setter +
/// `input`/`change`) rather than by `Input.insertText`, which Chromium ignores on
/// these controls — Playwright `fill()`'s set-value set. Parsed from the page's
/// own report of the input type, so it is a closed set: a page cannot smuggle an
/// arbitrary string into a receipt through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetValueType {
    Color,
    Date,
    DatetimeLocal,
    Month,
    Range,
    Time,
    Week,
}

impl SetValueType {
    pub const ALL: [Self; 7] = [
        Self::Color,
        Self::Date,
        Self::DatetimeLocal,
        Self::Month,
        Self::Range,
        Self::Time,
        Self::Week,
    ];

    pub fn parse(input_type: &str) -> Option<Self> {
        Some(match input_type {
            "color" => Self::Color,
            "date" => Self::Date,
            "datetime-local" => Self::DatetimeLocal,
            "month" => Self::Month,
            "range" => Self::Range,
            "time" => Self::Time,
            "week" => Self::Week,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Color => "color",
            Self::Date => "date",
            Self::DatetimeLocal => "datetime-local",
            Self::Month => "month",
            Self::Range => "range",
            Self::Time => "time",
            Self::Week => "week",
        }
    }

    /// The value format the input keeps verbatim (anything else is rejected or
    /// normalised by the browser, and so reads back different).
    pub fn expected_format(self) -> &'static str {
        match self {
            Self::Color => "#rrggbb in lower case",
            Self::Date => "yyyy-mm-dd",
            Self::DatetimeLocal => "yyyy-mm-ddThh:mm (seconds only when non-zero)",
            Self::Month => "yyyy-mm",
            Self::Range => "a number the input's min/max/step allow",
            Self::Time => "hh:mm, 24-hour (seconds only when non-zero)",
            Self::Week => "yyyy-Www",
        }
    }
}

/// Outcome of a `web.wait` locator poll (`send_wait`). An application outcome the
/// host maps to a typed wire receipt, distinct from transport `Err(LoomError)`:
/// - `Resolved` — the locator matched a node on some poll before the deadline.
/// - `PredicateFalse` — the deadline elapsed without the locator ever resolving
///   (surfaced to the CLI as the typed `kind: "wait_predicate_false"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResolveOutcome {
    Resolved,
    PredicateFalse,
}

/// Parsed result of a `Runtime.evaluate` CDP call. Exactly one of `result`
/// / `exception` is `Some` per CDP semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluateOutcome {
    /// On success: the raw CBOR `result.value`. None when the page threw.
    pub result: Option<ciborium::value::Value>,
    /// On exception: structured details. None on successful evaluation.
    pub exception: Option<EvaluateException>,
}

/// Structured page-side exception details from CDP `exceptionDetails`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluateException {
    /// `exceptionDetails.text` — usually `"Uncaught"`.
    pub text: String,
    /// Extracted exception message (`exception.description` or
    /// stringified `exception.value`). Used for the
    /// `details.exception` field on page-side throws.
    pub message: String,
    pub line: u32,
    pub column: u32,
}

// ─── Per-verb send_* params ──────────────────────────────────────────────────
//
// These group the call-varying arguments of the typed verb senders so the
// signatures clear clippy's `too_many_arguments` threshold without the
// per-fn `#[allow]`. `action_id` is the WASM-guest-computed action hash
// (sha256 of the action payload), threaded for host-side receipt correlation
// / observability (Q5); the shim never sees it. `seed` / `epoch_ms` ride the
// wire to the shim where they're rendered into the determinism JS template.

/// Params for [`ShimManager::send_navigate`].
///
/// `budget_ms` overrides the recv timeout when larger than the default.
/// `blocklist_enabled` toggles the shim's `Fetch.enable` interception path
/// (affirmative form on the wire so logs read directly). `until` is the
/// settle-capture readiness mode gating the capture; `determinism_enabled`
/// is the per-session determinism toggle.
#[derive(Debug, Clone)]
pub struct SendNavigateParams {
    pub id: ShimId,
    pub action_id: String,
    pub session_id: u64,
    pub target_id: u64,
    pub url: String,
    pub budget_ms: u64,
    pub seed: Seed,
    pub epoch_ms: EpochMs,
    pub blocklist_enabled: bool,
    pub until: String,
    pub determinism_enabled: bool,
    /// voice-call-io: per-session `--audio` opt-in carried onto the wire
    /// `PageNavigate.audio_enabled` (and the lazy-spawn `SpawnTarget`).
    pub audio_enabled: bool,
}

/// Params for [`ShimManager::send_wait_for`].
#[derive(Debug, Clone)]
pub struct SendWaitForParams {
    pub id: ShimId,
    pub action_id: String,
    pub session_id: u64,
    pub target_id: u64,
    pub until: String,
    pub budget_ms: u64,
    pub seed: Seed,
    pub epoch_ms: EpochMs,
    pub determinism_enabled: bool,
    /// voice-call-io: carried onto the idempotent lazy-spawn `SpawnTarget`.
    pub audio_enabled: bool,
}

/// Params for [`ShimManager::send_evaluate`].
#[derive(Debug, Clone)]
pub struct SendEvaluateParams {
    pub id: ShimId,
    pub action_id: String,
    pub session_id: u64,
    pub target_id: u64,
    pub expression: String,
    pub budget_ms: u64,
    pub seed: Seed,
    pub epoch_ms: EpochMs,
    pub determinism_enabled: bool,
    /// voice-call-io: carried onto the idempotent lazy-spawn `SpawnTarget`.
    pub audio_enabled: bool,
}

/// Params for [`ShimManager::send_set_input_files`].
#[derive(Debug, Clone)]
pub struct SendSetInputFilesParams {
    pub id: ShimId,
    pub action_id: String,
    pub session_id: u64,
    pub target_id: u64,
    pub selector: String,
    pub files: Vec<String>,
    pub budget_ms: u64,
    pub seed: Seed,
    pub epoch_ms: EpochMs,
    pub determinism_enabled: bool,
    /// voice-call-io: carried onto the idempotent lazy-spawn `SpawnTarget`.
    pub audio_enabled: bool,
}

/// Params for [`ShimManager::send_press_key`] (cdp-trusted-input). Bundled so
/// the signature clears clippy's `too_many_arguments` threshold, matching the
/// other multi-arg senders. `selector` is optional (focus-then-press; omit for
/// ambient focus); `modifiers` are Control/Alt/Shift/Meta.
#[derive(Debug, Clone)]
pub struct SendPressKeyParams {
    pub id: ShimId,
    pub session_id: u64,
    pub target_id: u64,
    pub key: String,
    pub selector: Option<String>,
    pub modifiers: Vec<String>,
    pub budget_ms: u64,
}
