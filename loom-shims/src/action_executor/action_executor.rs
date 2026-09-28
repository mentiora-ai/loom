// ActionExecutor — verb library + `cdp_send` worker.
//
// # Contract semantics
// - **R3 pre-condition.** Before dispatching any
//   `Page.navigate`, `ActionExecutor` calls
//   `TargetManager::determinism_ready(target_id)` and returns
//   `ShimErrorCode::ShimInternalError` (with detail
//   `"R3OrderingViolation"`) if the flag is still false.
// - **R1 pre-condition.** Before `Page.navigate`,
//   subscribes `NetworkInterceptor` for the target so the
//   `Network.responseReceived` events arrive before the response body
//   is evicted from Chromium's cache.
// - **`cdp_send` async.** Each `cdp_send` is its own
//   tokio task; multiple in-flight roundtrips do not serialise.
// - **No CDP payload escape.** `ActionExecutor`
//   translates CDP responses into typed `ActionResult` shapes.
//   `cdp_send` is the one path that round-trips opaque CBOR — the
//   daemon already routed that bytes-only via `loom-host::shim_call`,
//   never to a WASM caller.
// - **Per-target ordering for stateful actions.** Click → focus →
//   type sequence is enforced by serialising actions targeting the
//   same `target_id`; cross-target actions remain parallel.

use crate::cdp_connection::cdp_connection::{CdpConnection, CdpError};
use crate::ipc_endpoint::ipc_endpoint::{CdpMessage, ShimErrorCode, ShimResponse, TargetId};
use crate::network_interceptor::network_interceptor::{
    BlockedEvent, LoomNetworkEntry, LoomNetworkEvent, NetworkInterceptor,
};
use crate::target_manager::target_manager::TargetManager;
use async_trait::async_trait;
use ciborium::value::Value as CborValue;
use loom_shared::navigate_outcome::ShimConsoleLine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

/// Default per-action budget. Daemon may override per-call.
pub const DEFAULT_ACTION_BUDGET: Duration = Duration::from_secs(30);

/// Default budget specifically for `page_navigate`. Tighter than
/// `DEFAULT_ACTION_BUDGET` so an unreachable host (DNS failure,
/// connection refused, slow-TLS handshake) surfaces a typed-error
/// receipt within the network-error budget window. CDP itself fast-
/// fails DNS / connection-refused sub-second; this constant bounds
/// the slow-TLS / unresponsive-host worst case.
///
/// Overridable at process start via `LOOM_SHIM_CDP_TIMEOUT_MS` (see
/// [`navigate_budget`]) — production topologies running many orchestrator
/// instances can raise it so a slow-but-healthy navigate on a CPU-saturated
/// host isn't trapped as a timeout. The default is unchanged.
pub const DEFAULT_NAVIGATE_BUDGET: Duration = Duration::from_secs(10);

/// Env var (milliseconds) overriding the per-CDP-command navigate budget.
const NAVIGATE_BUDGET_ENV: &str = "LOOM_SHIM_CDP_TIMEOUT_MS";

/// Upper bound for the best-effort virtual-time RESUME on a navigate exit
/// (animation-capture Mode A). The resume is a single `setVirtualTimePolicy`
/// round-trip, so it should return sub-second on a healthy renderer; capping it
/// here keeps a failed navigate's cleanup from inheriting the full navigate budget
/// (which can be 10s+) and guarantees the cleanup can never itself wedge.
pub(crate) const RESUME_CDP_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard ceiling on a settle `timeout`, so computing the shared deadline
/// (`Instant::now() + timeout`) can never overflow the monotonic clock even if a
/// misconfigured/hostile budget arrives over the wire (click-cross-origin-until
/// ship FND-0006). Five minutes is far beyond any legitimate per-call settle
/// (the RPC per-call deadline is ~30s); a value above this is pinned here.
pub(crate) const MAX_SETTLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum number of unsolicited top-level navigations (client-side redirects)
/// the settle path will FOLLOW within a single navigate/wait_for before giving
/// up and returning the bounded `timeout` outcome (client-nav-reattach D4). A
/// page that bounces between login states forever (a redirect loop) is capped
/// here so the re-attach loop can never spin unbounded; the per-action wall-clock
/// deadline is the other, tighter bound. 10 covers real multi-step auth flows
/// (SPA shell → IdP → consent → app) with headroom.
pub(crate) const MAX_REATTACH_HOPS: u32 = 10;

/// Parse a `LOOM_SHIM_CDP_TIMEOUT_MS` value into a navigate budget. A missing,
/// non-numeric, or non-positive value falls back to [`DEFAULT_NAVIGATE_BUDGET`]
/// (a `0`/garbage knob must not disable the budget — that would re-introduce
/// the unbounded-navigate trap this guards against). Pure so the parse policy
/// is unit-testable without the process-global env or the cache below.
pub(crate) fn parse_navigate_budget(raw: Option<&str>) -> Duration {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_NAVIGATE_BUDGET)
}

/// Per-CDP-command navigate budget, read from `LOOM_SHIM_CDP_TIMEOUT_MS` once
/// per process and cached (mirrors `connection_handler::max_concurrent_requests`).
/// Used only when the caller passes no explicit `budget` — which the dispatcher
/// always does for `Page.navigate`, so this is the binding timeout in production.
pub(crate) fn navigate_budget() -> Duration {
    static CACHED: OnceLock<Duration> = OnceLock::new();
    *CACHED
        .get_or_init(|| parse_navigate_budget(std::env::var(NAVIGATE_BUDGET_ENV).ok().as_deref()))
}

/// Decide whether to RESUME (advance) the renderer's virtual clock on navigate
/// exit. The clock is left FROZEN at the drained budget horizon ONLY on the
/// determinism-pinned clean-drain path (`clock_pinned && budget_drained`), where
/// a later `web.evaluate` clock read must replay byte-equal. In EVERY other case
/// — determinism OFF (`clock_pinned == false`), or the budget did not cleanly
/// drain — the clock must be resumed: a paused virtual clock defers the NEXT
/// navigate's `Page.loadEventFired`, and the determinism-OFF navigate path awaits
/// load BEFORE re-arming a budget, so it would deadlock on the deferred load and
/// burn the full navigate budget every call (navigate-degradation regression;
/// the +20s-per-navigate wedge). `vt_active == false` means virtual time is
/// disabled outright (`LOOM_CAPTURE_VIRTUAL_TIME=0`) → nothing to resume.
///
/// Pure so the resume policy is unit-testable without a live renderer; the three
/// `page_navigate` exit guards (success + the two CDP-error bail paths) all route
/// through it so the policy can never drift between them.
pub(crate) fn should_resume_virtual_clock(
    vt_active: bool,
    clock_pinned: bool,
    budget_drained: bool,
) -> bool {
    vt_active && !(clock_pinned && budget_drained)
}

/// settle-capture: serde defaults for the readiness fields on
/// `ActionResult::Navigated`, so a pre-feature CBOR payload decodes unchanged.
fn default_settle_until_field() -> String {
    "settled".to_string()
}
fn default_settle_outcome_field() -> String {
    "reached".to_string()
}

/// Translated, typed result of an action. Never carries raw CDP bytes
/// outside `cdp_send`'s explicit pass-through path.
// Navigated carries dom_bytes + screenshot_bytes (heap Vec<u8>) alongside
// multiple Strings for the tier-2 receipt payload. All large fields are
// heap-allocated; the stack-frame size difference is pointer-width only.
// Boxing Navigated would add an extra allocation per navigate for no gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionResult {
    /// `page_navigate` completed.
    Navigated {
        target_id: TargetId,
        frame_id: String,
        loader_id: String,
        network_events: Vec<LoomNetworkEvent>,
        /// SHA-256 of the captured DOM snapshot (post-load). Hex.
        dom_after_sha256: String,
        /// SHA-256 of the captured screenshot (post-load). Hex. NOT
        /// part of the bit-equal replay hash chain.
        screenshot_sha256: String,
        // --- Tier-2 payload fields ---
        /// Requested URL (from PageNavigate.url).
        url: String,
        /// Final URL after redirects. Stub: same as url.
        final_url: String,
        /// Page title. Stub: empty string.
        page_title: String,
        /// HTTP status of main document. Derived from the main-document
        /// event (`main_document_event_index`); falls back to 0 when no
        /// main-document event was captured.
        status_code: u16,
        /// Index into `network_events` of the event attributed to THIS
        /// navigation's main document (matched against the
        /// `Page.navigate` response's loaderId/frameId; see
        /// `find_main_document_index`). `None` = no event attributable
        /// to the main document (no events, or only iframe documents).
        /// The host scopes the navigate failure verdict (4xx/transport)
        /// to this event ONLY — iframe document errors stay in
        /// `network_events` for observability without failing the
        /// navigate. Control-flow plumbing only: never enters the
        /// hashed receipt. `serde(default)` for CBOR wire back-compat.
        #[serde(default)]
        main_document_event_index: Option<u32>,
        /// Raw CBOR bytes of the DOM.getDocument response (dom_after_sha256 = sha256 of these).
        dom_bytes: Vec<u8>,
        /// Raw CBOR bytes of the Page.captureScreenshot response (screenshot_sha256 = sha256 of these).
        screenshot_bytes: Vec<u8>,
        /// Console lines captured by shim. Stub: always empty.
        console_lines: Vec<ShimConsoleLine>,
        /// Sub-resource requests blocked by the default blocklist.
        /// Drained from
        /// `NetworkInterceptor::drain_blocked` after `Page.loadEventFired`;
        /// the host writes one `AuditEntry { kind: BlockedUrl }` per
        /// event into the manifest hash chain. `serde(default)` so a
        /// pre-feature CBOR payload (no field) decodes as empty.
        #[serde(default)]
        blocked_events: Vec<BlockedEvent>,
        /// Raw per-request network entries (full-capture, observational —
        /// xhr/fetch/subresource/document) read from the `NetworkInterceptor`
        /// accumulator after `Page.loadEventFired`. NOT part of the replay
        /// hash chain. `serde(default)` so a pre-feature CBOR payload decodes
        /// with an empty vec.
        #[serde(default)]
        network_entries: Vec<LoomNetworkEntry>,
        /// True when the shim accumulator hit its cap and dropped entries.
        #[serde(default)]
        network_entries_truncated: bool,
        // --- settle-capture readiness fields ---
        /// Readiness mode the capture was gated on (`load|networkidle|settled`).
        #[serde(default = "default_settle_until_field")]
        settle_until: String,
        /// How the wait ended: `reached|timeout|dom_unstable`.
        #[serde(default = "default_settle_outcome_field")]
        settle_outcome: String,
        /// Virtual-tick count mapped to ms; diagnostic only (excluded from
        /// the host's outcome_hash).
        #[serde(default)]
        settle_ms: u64,
        /// In-flight (non-WS/SSE) request count at settle.
        #[serde(default)]
        network_count_at_settle: u64,
    },
    /// `cdp_send` round-trip; opaque pass-through to the daemon.
    CdpResult { result: CborValue },
    /// `page_close` completed.
    PageClosed { target_id: TargetId },
    /// settle-capture slice 2: `wait_for` completed — a standalone readiness
    /// wait on the current page (no navigation, no capture). Carries only the
    /// settle verdict; the host stamps `emitted_at_ms`.
    Waited {
        /// Readiness mode that was waited for (`load|networkidle|settled`).
        settle_until: String,
        /// How the wait ended: `reached|timeout|dom_unstable`.
        settle_outcome: String,
        /// Virtual-tick count mapped to ms; diagnostic only (excluded from the
        /// host's outcome_hash).
        settle_ms: u64,
        /// In-flight (non-WS/SSE) request count at settle.
        network_count_at_settle: u64,
    },
}

/// Concrete ActionExecutor.
pub struct ChromiumActionExecutor {
    pub(crate) cdp: Arc<dyn CdpConnection>,
    pub(crate) network: Arc<dyn NetworkInterceptor>,
    pub(crate) targets: Arc<dyn TargetManager>,
    pub(crate) default_budget: Duration,
    /// video-capture: per-target screencast recordings. Shares `cdp` so the
    /// recorder issues `Page.startScreencast`/`Ack`/`stopScreencast` on the
    /// same connection.
    pub(crate) recorder: Arc<crate::screencast_recorder::ScreencastRecorder>,
    /// voice-call-io (task 04): synthetic-microphone audio inject. Shares `cdp`
    /// (issues `Runtime.evaluate`/`callFunctionOn`) and `targets` (reads the
    /// per-target `audio_nonce` to address the in-page enqueue hook).
    pub(crate) audio: Arc<crate::audio_bridge::AudioBridge>,
}

impl ChromiumActionExecutor {
    pub fn new(
        cdp: Arc<dyn CdpConnection>,
        network: Arc<dyn NetworkInterceptor>,
        targets: Arc<dyn TargetManager>,
    ) -> Self {
        let recorder = Arc::new(crate::screencast_recorder::ScreencastRecorder::new(
            cdp.clone(),
            Arc::new(crate::screencast_recorder::FfmpegSidecarEncoder),
        ));
        let audio = Arc::new(crate::audio_bridge::AudioBridge::new(
            cdp.clone(),
            targets.clone(),
        ));
        Self {
            cdp,
            network,
            targets,
            default_budget: DEFAULT_ACTION_BUDGET,
            recorder,
            audio,
        }
    }
}

/// Public ActionExecutor trait surface. All methods are async so the
/// dispatcher can drive multiple in-flight CDP roundtrips concurrently
/// and so that L4's `chromiumoxide::Browser` calls — which
/// are inherently async — fit cleanly into the call chain.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Pass-through CDP command. p99 ≤ 50ms.
    /// Errors: `CdpTimeout`, `CdpProtocolError`, `TargetUnknown`.
    async fn cdp_send(
        &self,
        target_id: TargetId,
        msg: CdpMessage,
        budget: Option<Duration>,
    ) -> Result<ActionResult, ShimResponse>;

    /// Navigate target to URL. R3 pre-check + R1 subscription before
    /// `Page.navigate`. Drains `LoomNetworkEvent`s from `NetworkInterceptor`
    /// after `Page.loadEventFired`.
    ///
    /// `blocklist_enabled` — when true,
    /// also issues `Fetch.enable` before navigate so sub-resources are
    /// gated against the default blocklist; drained `BlockedEvent`s
    /// land in the receipt's `blocked_events` field and become
    /// manifest `AuditEntry { kind: BlockedUrl }` on the host side.
    /// When false, no `Fetch.enable` is sent and `blocked_events` is
    /// empty.
    ///
    /// Errors: `R3OrderingViolation` if `determinism_injected == false`;
    /// `CdpTimeout` if budget exceeded.
    // 8 args (incl. audio_enabled): the per-navigate wire fields are passed
    // positionally to mirror the ShimRequest::PageNavigate variant; a params
    // struct would just re-wrap the same fields.
    #[allow(clippy::too_many_arguments)]
    async fn page_navigate(
        &self,
        target_id: TargetId,
        url: String,
        budget: Option<Duration>,
        blocklist_enabled: bool,
        // settle-capture: readiness state the capture is gated on.
        settle_mode: crate::readiness_monitor::SettleMode,
        // cross-run determinism: when true (determinism on), await the
        // virtual-time budget to drain before DOM capture so timer-driven DOM
        // mutations have deterministically fired. `--no-determinism` → false.
        determinism_enabled: bool,
        // voice-call-io (task 03): when true (`--audio` session), grant
        // `audioCapture` scoped to this navigation's origin (D16) before issuing
        // `Page.navigate`, so the app's first `getUserMedia({audio})` proceeds
        // without a prompt. Best-effort — a grant failure only WARNs.
        audio_enabled: bool,
    ) -> Result<ActionResult, ShimResponse>;

    /// settle-capture slice 2: run a standalone readiness wait on `target_id`
    /// (no navigation, no capture), reusing the SettleDriver/ReadinessMonitor.
    /// Returns `ActionResult::Waited` with the settle verdict. Bounded by the
    /// tick ceiling derived from `budget` — returns a typed `timeout` /
    /// `dom_unstable` verdict rather than hanging.
    async fn wait_for(
        &self,
        target_id: TargetId,
        settle_mode: crate::readiness_monitor::SettleMode,
        budget: Option<Duration>,
    ) -> Result<ActionResult, ShimResponse>;

    /// Close the target.
    async fn page_close(&self, target_id: TargetId) -> Result<ActionResult, ShimResponse>;

    /// Read (NON-draining) the full-capture network-entries snapshot for the
    /// target — everything observed since the last navigate. Backs the
    /// `loom.web.network_log` tool. Returns `(entries, shim_truncated)`.
    /// Default returns empty (only `ChromiumActionExecutor` accumulates).
    fn read_network_log(&self, _target_id: TargetId) -> (Vec<LoomNetworkEntry>, bool) {
        (Vec::new(), false)
    }

    /// video-capture: start a `Page.startScreencast` recording on `target_id`.
    /// `Err(detail)` (no session abort) when a recording is already active, the
    /// kill-switch is set, or `Page.startScreencast` fails. Default impl (non-
    /// Chromium executors) reports unsupported.
    async fn start_recording(
        &self,
        _target_id: TargetId,
        _caps: crate::screencast_recorder::Caps,
    ) -> Result<(), String> {
        Err("screencast recording not supported by this executor".to_string())
    }

    /// video-capture: stop the active recording, encode it, and return the
    /// outcome (errors are embedded in the outcome, never at the call boundary).
    async fn stop_recording(
        &self,
        _target_id: TargetId,
    ) -> loom_shared::navigate_outcome::ScreencastOutcome {
        loom_shared::navigate_outcome::ScreencastOutcome {
            stop_reason: "error".to_string(),
            error: Some("screencast recording not supported by this executor".to_string()),
            ..Default::default()
        }
    }

    /// voice-call-io (task 04): inject daemon-resolved audio bytes into the
    /// target's synthetic microphone via the in-page enqueue hook. `Err(typed_kind)`
    /// (no session abort) on a page rejection / timeout / audio-not-enabled. Default
    /// impl (non-Chromium executors) reports unsupported.
    async fn inject_audio(
        &self,
        _session_id: loom_shared::shim_protocol::SessionId,
        _target_id: TargetId,
        _bytes: Vec<u8>,
        _await_playout: bool,
    ) -> Result<loom_shared::navigate_outcome::AudioInjectOutcome, String> {
        Err("audio inject not supported by this executor".to_string())
    }

    /// voice-call-io (task 06): start capturing inbound (remote) WebRTC audio on
    /// the target. Fire-and-confirm; `Err(typed_kind)` (no session abort) on
    /// audio-not-enabled / double-start. Default impl reports unsupported.
    async fn start_audio_capture(
        &self,
        _session_id: loom_shared::shim_protocol::SessionId,
        _target_id: TargetId,
        _caps: crate::audio_bridge::Caps,
    ) -> Result<(), String> {
        Err("audio capture not supported by this executor".to_string())
    }

    /// voice-call-io (task 06): stop the active capture, drain+resample+WAV-mux, and
    /// return the outcome (errors embedded, never at the call boundary — mirrors
    /// `stop_recording`). Default impl reports unsupported.
    async fn stop_audio_capture(
        &self,
        _session_id: loom_shared::shim_protocol::SessionId,
        _target_id: TargetId,
    ) -> loom_shared::navigate_outcome::AudioCaptureOutcome {
        loom_shared::navigate_outcome::AudioCaptureOutcome {
            stop_reason: "error".to_string(),
            error: Some("audio capture not supported by this executor".to_string()),
            ..Default::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("R3 ordering violation: navigate before determinism inject on target {0}")]
    R3OrderingViolation(TargetId),
    #[error("CDP failure: {0}")]
    Cdp(#[from] CdpError),
    #[error("target {0} unknown")]
    TargetUnknown(TargetId),
}

impl From<ActionError> for ShimErrorCode {
    fn from(e: ActionError) -> Self {
        match e {
            ActionError::R3OrderingViolation(_) => ShimErrorCode::ShimInternalError,
            ActionError::Cdp(c) => c.into(),
            ActionError::TargetUnknown(_) => ShimErrorCode::TargetUnknown,
        }
    }
}

#[async_trait]
impl ActionExecutor for ChromiumActionExecutor {
    async fn cdp_send(
        &self,
        target_id: TargetId,
        msg: CdpMessage,
        budget: Option<Duration>,
    ) -> Result<ActionResult, ShimResponse> {
        // Snapshot (and the other guest verbs) reach DOM.getDocument via
        // this generic path and hash the raw response; normalize it here too so
        // their `dom_snapshot_hash` is content-stable, just like navigate STEP 5.
        let is_dom_get_document = msg.method == "DOM.getDocument";
        match self.cdp.command(target_id, msg, budget).await {
            Ok(result) => {
                let result = if is_dom_get_document {
                    let normalized =
                        loom_shared::dom_normalize::normalize_dom_cbor(&cbor_to_bytes(&result));
                    ciborium::de::from_reader(normalized.as_bytes()).unwrap_or(result)
                } else {
                    result
                };
                Ok(ActionResult::CdpResult { result })
            }
            Err(e) => Err(action_error_to_response(ActionError::Cdp(e), 0, None)),
        }
    }

    /// See [`ChromiumActionExecutor::page_navigate_impl`].
    async fn page_navigate(
        &self,
        target_id: TargetId,
        url: String,
        budget: Option<Duration>,
        blocklist_enabled: bool,
        settle_mode: crate::readiness_monitor::SettleMode,
        determinism_enabled: bool,
        audio_enabled: bool,
    ) -> Result<ActionResult, ShimResponse> {
        self.page_navigate_impl(
            target_id,
            url,
            budget,
            blocklist_enabled,
            settle_mode,
            determinism_enabled,
            audio_enabled,
        )
        .await
    }

    /// See [`ChromiumActionExecutor::wait_for_impl`].
    async fn wait_for(
        &self,
        target_id: TargetId,
        settle_mode: crate::readiness_monitor::SettleMode,
        budget: Option<Duration>,
    ) -> Result<ActionResult, ShimResponse> {
        self.wait_for_impl(target_id, settle_mode, budget).await
    }

    async fn page_close(&self, target_id: TargetId) -> Result<ActionResult, ShimResponse> {
        // voice-call-io (task 03): on the CLEAN close path, reset the granted
        // `audioCapture` permission (D16/FND-0006) BEFORE closing the target.
        // Only when this session actually granted it. Browser-scope, best-effort
        // — a failure only WARNs. On a crash/abort the whole browser context is
        // torn down instead, so the grant dies with it (no cross-session leak).
        if crate::cdp_connection::cdp_connection::audio_capture_granted() {
            let reset_msg = CdpMessage {
                method: crate::cdp_connection::cdp_connection::RESET_PERMISSIONS_METHOD.to_string(),
                params: crate::cdp_connection::cdp_connection::build_reset_permissions_params(),
            };
            if let Err(e) = self.cdp.command(target_id, reset_msg, None).await {
                tracing::warn!(
                    target_id,
                    error = %e,
                    "audio: Browser.resetPermissions on close failed (best-effort; the browser \
                     context teardown clears the grant regardless)"
                );
            } else {
                tracing::info!(target_id, "audio: Browser.resetPermissions issued on close");
            }
        }

        // Target.closeTarget over CDP. Best-effort — even if the CDP call
        // fails (Chromium already shut down), the upstream caller treats
        // the target as gone.
        let close_msg = CdpMessage {
            method: "Target.closeTarget".into(),
            params: CborValue::Map(vec![(
                CborValue::Text("targetId".into()),
                CborValue::Text(format!("{target_id}")),
            )]),
        };
        let _ = self.cdp.command(target_id, close_msg, None).await;
        Ok(ActionResult::PageClosed { target_id })
    }

    fn read_network_log(&self, target_id: TargetId) -> (Vec<LoomNetworkEntry>, bool) {
        // CDP events accumulate under target_id==0 (see page_navigate drain
        // note); fall back to the explicit target for the fake-chromium
        // per-target path.
        let from_zero = self.network.read_entries(0);
        if from_zero.0.is_empty() {
            self.network.read_entries(target_id)
        } else {
            from_zero
        }
    }

    async fn start_recording(
        &self,
        target_id: TargetId,
        caps: crate::screencast_recorder::Caps,
    ) -> Result<(), String> {
        self.recorder.start(target_id, caps).await
    }

    async fn stop_recording(
        &self,
        target_id: TargetId,
    ) -> loom_shared::navigate_outcome::ScreencastOutcome {
        self.recorder.stop(target_id).await
    }

    async fn inject_audio(
        &self,
        session_id: loom_shared::shim_protocol::SessionId,
        target_id: TargetId,
        bytes: Vec<u8>,
        await_playout: bool,
    ) -> Result<loom_shared::navigate_outcome::AudioInjectOutcome, String> {
        self.audio
            .inject(session_id, target_id, &bytes, await_playout)
            .await
    }

    async fn start_audio_capture(
        &self,
        session_id: loom_shared::shim_protocol::SessionId,
        target_id: TargetId,
        caps: crate::audio_bridge::Caps,
    ) -> Result<(), String> {
        self.audio.start_capture(session_id, target_id, caps).await
    }

    async fn stop_audio_capture(
        &self,
        session_id: loom_shared::shim_protocol::SessionId,
        target_id: TargetId,
    ) -> loom_shared::navigate_outcome::AudioCaptureOutcome {
        self.audio.stop_capture(session_id, target_id).await
    }
}

/// Serialize a CBOR value to raw bytes. Used to populate dom_bytes / screenshot_bytes.
pub(crate) fn cbor_to_bytes(value: &CborValue) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = ciborium::ser::into_writer(value, &mut bytes);
    bytes
}

/// Compute SHA-256 (lowercase hex) of raw bytes. Used for `screenshot_sha256`
/// now that the screenshot is stored as a decoded PNG rather than its CBOR
/// envelope.
pub(crate) fn sha256_hex_of_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Pure helper: convert an `ActionError` into the typed
/// `ShimResponse::Error` envelope. Centralised so all error returns
/// have consistent detail strings. `request_id` is supplied by the
/// caller (typically the dispatcher echoing the originating request id).
pub fn action_error_to_response(
    err: ActionError,
    request_id: u64,
    session_id: Option<u64>,
) -> ShimResponse {
    let detail = err.to_string();
    let code: ShimErrorCode = err.into();
    ShimResponse::Error {
        request_id,
        session_id,
        code,
        detail,
    }
}
