//! `WasmHost` → `WasmHostBridge` bridge + host-bridge construction.
//!
//! Split out of `lib.rs` (large-file refactor), unchanged:
//!   * `StubHostBridge` — returns `SurfaceUnavailable` until `loom
//!     postinstall` compiles the WASM modules.
//!   * `WasmBridge` + its `WasmHostBridge` impl — the real dispatch path:
//!     terminal-status reject, the per-session dispatch fence, the
//!     safe-profile evaluate gate, cookie-grant resolution + per-cookie
//!     validation, the upload allow-list gate, the host/shim-side reads
//!     (network_log / start_recording / stop_recording), payload encoding,
//!     and the `host.dispatch` → wire-receipt round trip.
//!   * `build_host_bridge` — resolves surfaces + Chromium and builds the
//!     real `WasmBridge` (or the stub on load failure). Called by `async_main`.
//!
//! Receipt/payload builders live in `crate::wire_receipts`; the dispatch
//! fence + activity guard live in `crate::core_bridge`.

use crate::core_bridge::{acquire_dispatch_slot, ActionActivityGuard};
use crate::guest_args::*;
use crate::media_receipts::*;
use crate::navigate_receipt::*;
use crate::wire_receipts::*;
use crate::{map_loom_error, now_epoch_ms, upload_guard};
use loom_core::core_api_facade::CoreApiFacade;
use loom_rpc::host_service_adapter::host_service_adapter::{
    Action, AdapterError as HostAdapterError, Receipt, WasmHostBridge,
};
use std::path::PathBuf;
use std::sync::Arc;

// ─── Bridge: WasmHost → WasmHostBridge ──────────────────────────────────────

/// interactive-settle-bounded: headroom subtracted from the effective deadline
/// when clamping the interaction settle budget, so the bounded settle always
/// returns a receipt BEFORE the logical deadline rather than racing it. Sized to
/// cover the shim's post-settle virtual-time RESUME cleanup (`RESUME_CDP_TIMEOUT`
/// = 2s in `loom-shims`, which runs AFTER the settle verdict and can add up to
/// that much wall-clock) plus ~1s margin — so `settle_budget + resume` still
/// lands inside the deadline (click-cross-origin-until ship FND-0001/0009). If
/// `RESUME_CDP_TIMEOUT` changes, revisit this.
const SETTLE_HEADROOM_MS: u64 = 3_000;

// Compile-time guard: the headroom must cover the shim's post-settle resume
// cleanup (`loom-shims` `RESUME_CDP_TIMEOUT` = 2s, which runs AFTER the settle
// verdict) so `settle_budget + resume` still lands inside the deadline (ship
// FND-0001). If RESUME_CDP_TIMEOUT grows past 2s, this breaks the build until
// the headroom is raised to match.
const _: () = assert!(SETTLE_HEADROOM_MS >= 2_000);

/// interactive-settle-bounded: the wall-clock budget for the post-action settle
/// run after a trusted-input dispatch. Mirrors navigate's budget model
/// (`LOOM_SHIM_CDP_TIMEOUT_MS` env → 10s default) so the settle is bounded
/// strictly inside the RPC deadline — a completed click can therefore never
/// surface a transport `rpc timeout`, only a typed `settle_outcome` receipt.
///
/// `deadline_ms` follows loom's session-executor convention: `None` **and
/// `Some(0)`** both mean "no deadline" (`session_executor.rs`) → the base budget
/// (still capped by the server per-call timeout). When the caller passes a REAL
/// (non-zero) deadline, clamp to the budget REMAINING inside the EFFECTIVE
/// deadline (`min(deadline_ms, server_cap)`) after the input dispatch already
/// consumed `elapsed_ms`, minus headroom, so the settle + its cleanup finish
/// before the deadline instead of racing it. There is NO lower floor: a
/// near-exhausted deadline yields a correspondingly small (or zero) budget rather
/// than being floored UP past the deadline — flooring up is exactly the bug that
/// re-tripped `request_timeout` (ship FND-0009). A zero budget returns an
/// immediate typed `timeout` in the shim. (The budget is a record-time wall-clock
/// bound, excluded from the hash chain, exactly like navigate's — NFR-DET-01.)
pub(crate) fn interaction_settle_budget_ms(deadline_ms: Option<u64>, elapsed_ms: u64) -> u64 {
    let base = std::env::var("LOOM_SHIM_CDP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SETTLE_BASE_MS);
    // `server_cap` is loom-rpc's EFFECTIVE per-call timeout (env-aware), read
    // from the single source `connection_handler::request_timeout_ms()` — no
    // duplicated env read / default that could drift (ship FND-0005/0019/0016).
    let server_cap = loom_rpc::connection_handler::request_timeout_ms();
    clamp_settle_budget(base, server_cap, deadline_ms, elapsed_ms)
}

/// interactive-settle-bounded: floor/margin for the trusted-input DISPATCH
/// budget. A real CDP `Input.*` ack is a browser-process round-trip (several to
/// tens of ms), so the dispatch budget MUST sit comfortably above that — a
/// sub-ms budget loses the mouse-ack race on every normal click and reports
/// `click_failed`. This is also the margin the bound leaves under the effective
/// deadline, so a genuinely swapped dispatch's typed return still beats the RPC
/// abandon. A swapped click therefore returns `DispatchedAckPending` after ~1×
/// budget (≥ this margin) — bounded, far under the 30s recv floor.
const DISPATCH_ACK_MARGIN_MS: u64 = 1_000;

/// interactive-settle-bounded: the wall-clock budget for the trusted-input
/// DISPATCH itself (the `Input.*` frames), so a click/type/press whose committing
/// event triggers a cross-origin top-level navigation can never dead-wait the
/// shim's recv floor (~30s) waiting for an ack the swapped-away renderer will
/// never send — the pre-fix bug where the dispatch passed `budget_ms=0`.
///
/// Bounded inside the effective per-call deadline (`min(deadline_ms, server_cap)`;
/// `None`/`Some(0)` ⇒ no caller deadline ⇒ the base budget), capped at the base.
/// **It does NOT reuse [`interaction_settle_budget_ms`]:** the settle formula
/// subtracts a 3s *resume* headroom (for the post-settle virtual-time resume, not
/// the input ack), which would collapse the dispatch budget to ~1ms for any
/// `deadline_ms ≤ 3s` — and a 1ms recv loses the ack race on real Chrome, so a
/// normal same-origin click with a common 1–3s deadline would fail `click_failed`.
/// Instead the dispatch budget is floored to [`DISPATCH_ACK_MARGIN_MS`] (above
/// real ack latency) and leaves only that margin under the deadline. The settle
/// budget IS elapsed-aware, so `dispatch + settle` still fits the deadline. A
/// record-time wall-clock bound, excluded from the hash chain like the settle
/// budget (NFR-DET-01).
pub(crate) fn interaction_dispatch_budget_ms(deadline_ms: Option<u64>) -> u64 {
    let base = std::env::var("LOOM_SHIM_CDP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SETTLE_BASE_MS);
    let server_cap = loom_rpc::connection_handler::request_timeout_ms();
    let effective_deadline = match deadline_ms {
        None | Some(0) => server_cap,
        Some(d) => d.min(server_cap),
    };
    effective_deadline
        .saturating_sub(DISPATCH_ACK_MARGIN_MS)
        .min(base)
        .max(DISPATCH_ACK_MARGIN_MS)
}

/// interactive-settle-bounded default base (`LOOM_SHIM_CDP_TIMEOUT_MS` unset).
const DEFAULT_SETTLE_BASE_MS: u64 = 10_000;

/// Pure core of [`interaction_settle_budget_ms`] — `server_cap` resolved by the
/// caller so this is deterministically unit-testable without process-global env
/// races.
///
/// Clamps the settle budget strictly inside the EFFECTIVE per-call deadline =
/// `min(caller deadline_ms, server_cap)`, minus headroom (which reserves room for
/// the shim's post-settle resume cleanup). The connection handler abandons the
/// RPC at that same effective deadline and returns a transport `request_timeout`,
/// so the budget — AND its floor — must never exceed it: a caller `deadline_ms`
/// larger than the cap (the reported 45s-vs-30s case) is bounded by the cap; a
/// caller `deadline_ms` TIGHTER than the cap bounds the floor too, so a small
/// deadline can't be floored UP past its own budget and re-trip request_timeout
/// (ship FND-0009/0004/0011). All math `saturating_*` (FND-0006) — no underflow.
fn clamp_settle_budget(
    base: u64,
    server_cap: u64,
    deadline_ms: Option<u64>,
    elapsed_ms: u64,
) -> u64 {
    // Effective deadline: the tighter of the caller's deadline and the server
    // cap. `None`/`Some(0)` ⇒ no caller deadline (loom convention) → server cap.
    let effective_deadline = match deadline_ms {
        None | Some(0) => server_cap,
        Some(d) => d.min(server_cap),
    };
    let hard = effective_deadline
        .saturating_sub(elapsed_ms)
        .saturating_sub(SETTLE_HEADROOM_MS);
    // No lower floor: `min(base, hard)` already never exceeds the effective
    // deadline, and a deliberate `hard == 0` (out of time) must stay 0 — flooring
    // it UP to a minimum is exactly the FND-0009 bug (settle overruns the caller's
    // deadline → transport request_timeout). The shim treats a 0 budget as an
    // immediate typed `timeout` (no CDP with a zero deadline).
    base.min(hard)
}

/// interactive-settle-bounded: run the bounded post-action readiness wait after
/// a SUCCESSFUL trusted-input dispatch and fold the verdict onto `receipt`.
/// Reuses the proven, wall-clock-bounded `web.wait_for` settle path
/// (`WasmHost::settle_after_input` → `send_wait_for` → `wait_for_settle`),
/// arming virtual time under the session determinism toggle. Best-effort: the
/// input already dispatched, so a settle transport failure logs and leaves the
/// receipt without a `settle_outcome` rather than failing the verb. The budget
/// is bounded strictly inside the RPC deadline, so the verb can never surface a
/// transport `rpc timeout` — only a typed `settle_outcome`.
#[allow(clippy::too_many_arguments)]
fn settle_after_input_dispatch(
    handle: &tokio::runtime::Handle,
    host: &Arc<loom_host::WasmHost>,
    session: &loom_core::session_manager::Session,
    session_id: &str,
    until: Option<&str>,
    deadline_ms: Option<u64>,
    dispatch_elapsed_ms: u64,
    receipt: &mut Receipt,
) {
    let budget = interaction_settle_budget_ms(deadline_ms, dispatch_elapsed_ms);
    let until = until.unwrap_or("settled");
    match handle.block_on(host.settle_after_input(
        session_id,
        until,
        budget,
        session.seed,
        session.epoch_ms,
        !session.no_determinism,
        session.audio,
    )) {
        Ok(outcome) => {
            // Redacted diagnostics: readiness verdict + timing, never page content.
            tracing::debug!(
                session_id = %session_id,
                until = %until,
                settle_outcome = %outcome.settle_outcome,
                settle_ms = outcome.settle_ms,
                network_count = outcome.network_count_at_settle,
                "interactive verb post-action settle complete"
            );
            stamp_settle_outcome(receipt, &outcome);
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "interactive verb post-action settle failed (input still dispatched)"
            );
        }
    }
}

/// Stub host bridge — returns `SurfaceUnavailable` for every action
/// dispatch until WASM modules are compiled by `loom postinstall`.
/// Replaced by a real `WasmHost`-backed impl once modules are present.
struct StubHostBridge;

impl WasmHostBridge for StubHostBridge {
    fn dispatch_action_blocking(
        &self,
        _action: Action,
        _deadline_ms: Option<u64>,
    ) -> Result<Receipt, HostAdapterError> {
        use loom_rpc::error_translator::error_translator::LoomErrorCode;
        Err(LoomErrorCode::SurfaceUnavailable)
    }

    // stub bridge means WASM host failed to load (no surfaces).
    // Reporting false here doesn't change behavior — `dispatch_action`
    // already errors with SurfaceUnavailable — but it produces a clearer
    // BrowserNotFound message at session.create when the surfaces dir
    // happens to be empty AND chromium is missing.
    fn has_chromium(&self) -> bool {
        false
    }
}

/// Real bridge wrapping `Arc<loom_host::WasmHost>`. Uses
/// `tokio::task::block_in_place` + `Handle::block_on` so the async
/// dispatch call is safely driven from a sync bridge method.
struct WasmBridge {
    host: Arc<loom_host::WasmHost>,
    core: Arc<CoreApiFacade>,
    /// was a `ShimChromiumConfig` registered at host boot?
    /// Set at `build_host_bridge` time from the resolver's outcome.
    has_chromium: bool,
    /// Allow-list root for `web.set_input_files` (from `LOOM_UPLOAD_ROOT`).
    /// `None` → uploads fail closed. Daemon-global (all sessions), per
    /// plan-council FND#4 — not threaded per-session.
    upload_root: Option<PathBuf>,
}

impl WasmHostBridge for WasmBridge {
    fn dispatch_action_blocking(
        &self,
        action: Action,
        deadline_ms: Option<u64>,
    ) -> Result<Receipt, HostAdapterError> {
        use loom_host::session_executor::{Action as HostAction, ActionOutcome, SessionHandle};
        use loom_rpc::error_translator::error_translator::LoomErrorCode;

        // Owned copy so the borrow doesn't outlive the move-out of
        // `action` further down (v0.9.7 follow-up rewrites action
        // mid-dispatch for cookie grant resolution).
        let session_id_str_owned = action_session_id(&action).to_string();
        let session_id_str = session_id_str_owned.as_str();

        // Resolve the session from core.
        let session = self
            .core
            .session_manager
            .get(loom_core::manifest_writer::SessionId(
                session_id_str.to_string(),
            ))
            .map_err(|_| LoomErrorCode::SessionNotFound)?;

        // Reject terminal sessions before any host dispatch.
        // The check must precede host.dispatch() so the shim is never reached.
        {
            use loom_core::budget_enforcer::KillReason;
            use loom_core::session_manager::SessionStatus;
            let status = *session.status.lock();
            match status {
                SessionStatus::Closed => return Err(LoomErrorCode::SessionClosed),
                SessionStatus::Aborted | SessionStatus::Killed | SessionStatus::Crashed => {
                    // Distinguish budget-driven kills from user/store aborts so
                    // the typed `budget-exceeded` code reaches the wire — the
                    // `kill_reason` was written by the budget kill callback
                    // before status flipped, so we just have to consult it.
                    return Err(match session.kill_reason.lock().as_ref() {
                        Some(KillReason::BudgetExceeded { .. }) => LoomErrorCode::BudgetExceeded,
                        _ => LoomErrorCode::SessionAborted,
                    });
                }
                SessionStatus::Created | SessionStatus::Active => {}
            }
        }

        // Per-session dispatch fence: serialize surface-verb dispatch per
        // session and fail fast (`too_many_requests`) while a previous —
        // possibly timeout/cancel-abandoned — dispatch is still running.
        // Held for the WHOLE blocking dispatch; released on every return
        // path via the guard's Drop. Placed AFTER the terminal-status
        // check (cheap rejection first) and BEFORE the activity guard so
        // a fenced-out action never touches `in_flight`.
        let _slot = acquire_dispatch_slot(&session)?;

        // Activity tracking for the idle reaper: mark the action in-flight for the WHOLE
        // dispatch (so an actively-working session is never seen as idle, even a single long
        // navigate) and bump `last_activity` on both entry and exit. The guard's Drop runs on
        // every return path — early returns, errors, panics-as-unwind — so `in_flight` can't
        // leak. Placed AFTER the terminal-status check so closed/aborted sessions don't count.
        session.action_started(now_epoch_ms());
        let _activity = ActionActivityGuard(Arc::clone(&session));

        // Safe profile blocks destructive evaluate.
        // Daemon-layer gate (NOT shim) — daemon has typed Action +
        // Session.profile in scope so we can short-circuit before
        // host.dispatch.
        //
        // NOTE: when adding new destructive verbs (e.g. `web.execute_storage_set`),
        // extend this match. The catchall is intentional — non-evaluate
        // actions pass through unchanged, but a future verb that should
        // be safe-gated will silently bypass this block until added here.
        match &action {
            Action::WebEvaluate { expression, .. } if session.profile == "safe" => {
                // `find_denylist_match` = raw substring pass + a
                // comment/whitespace-stripped pass (catches
                // `document . cookie` / `eval/**/(…)` smuggling). It is
                // a guardrail, not a sandbox — see the threat-model note
                // on `loom_shared::safety::EVALUATE_DENYLIST`.
                if let Some(matched) = loom_shared::safety::find_denylist_match(expression) {
                    tracing::warn!(
                        session_id = %session_id_str,
                        profile = "safe",
                        matched_pattern = matched,
                        "blocked destructive evaluate"
                    );
                    return Ok(profile_restricted_evaluate_receipt(
                        session.allocate_action_id(),
                        session_id_str,
                        matched,
                    ));
                }
            }
            _ => {}
        }

        // v0.9.7 follow-up A — Grant resolution + Follow-up B — per-cookie
        // validation, both performed daemon-side: under the settled
        // daemon-owns-verbs architecture the WASM guest forwards opaquely and
        // the daemon owns the safety/cookie checks (cookie types live in
        // `loom_shared::cookie_types`). Together they
        // bring the cookie verbs up to the user-facing contract
        // documented in the v0.9.6 task spec:
        //
        //   - `set_cookies` with `CookieSource::Grant` resolves through
        //     `core.vault.substitute_cookies(grant_id, session_id)` and
        //     dispatches with the resolved cookie array as if the
        //     operator had supplied `CookieSource::Inline { cookies }`.
        //   - Per-cookie validation (`validate_cookie_params`: 64-cap,
        //     name/value/expires) runs BEFORE the CDP envelope is
        //     built. Validation failures short-circuit to a typed
        //     `cookie_validation_error` receipt (matching the
        //     existing WASM-side ErrorMapper wire kind and the
        //     action_registry --help docstring) without ever
        //     touching the chromium shim.
        //
        // Shape errors (malformed `source` JSON, missing `cookies` key
        // in the vault blob) flow through the existing
        // `SchemaViolation` / `InternalError` codes — only typed
        // `CookieValidationError` variants flow through the
        // taxonomy-coded `cookie_validation_error` receipt. This split
        // keeps the wire taxonomy a closed set the operator can
        // group dashboards on.
        let action = match action {
            Action::WebSetCookies { session_id, source } => {
                use loom_core::manifest_writer::SessionId;
                use loom_core::vault::GrantId;
                use loom_shared::cookie_types::CookieSource;

                // Step 1: parse the `source` payload into the typed
                // `CookieSource` enum so malformed shapes (missing
                // `source` tag, unknown variants, wrong-typed fields)
                // fail closed via `SchemaViolation` rather than
                // silently falling through to the no-op chromium-args
                // branch with an empty cookies array.
                let typed_source: CookieSource =
                    serde_json::from_value(source).map_err(|_| LoomErrorCode::SchemaViolation)?;

                // Step 2: resolve a `Grant` to its cookies array by
                // calling `Vault::substitute_cookies`. The vault blob
                // schema is the canonical
                // `{"schema_version":1,"cookies":[NetworkCookieParam...]}`
                // produced by `loom vault add --credential-type cookie`;
                // a missing or non-array `cookies` field means the
                // keychain blob is corrupt and we fail closed with
                // `InternalError` rather than silently emitting an
                // empty Network.setCookies envelope.
                let cookies: Vec<loom_shared::cookie_types::NetworkCookieParam> = match typed_source
                {
                    CookieSource::Inline { cookies } => cookies,
                    CookieSource::Grant { grant_id } => {
                        let bytes = self
                            .core
                            .vault
                            .substitute_cookies(GrantId(grant_id), SessionId(session_id.clone()))
                            .map_err(|e| map_loom_error(&e))?;

                        #[derive(serde::Deserialize)]
                        struct VaultCookieBlob {
                            cookies: Vec<loom_shared::cookie_types::NetworkCookieParam>,
                        }

                        serde_json::from_slice::<VaultCookieBlob>(&bytes)
                            .map_err(|e| {
                                tracing::error!(
                                    "vault.substitute_cookies blob deserialise failed: {e}"
                                );
                                LoomErrorCode::InternalError
                            })?
                            .cookies
                    }
                };

                // Step 3: per-cookie validation. Failures short-circuit
                // to a typed `cookie_validation` receipt with the
                // snake_case taxonomy code from `cookie_validation_code`.
                if let Err(e) = loom_shared::cookie_types::validate_cookie_params(&cookies) {
                    return Ok(cookie_validation_error_receipt(
                        session.allocate_action_id(),
                        session_id_str,
                        cookie_validation_code(&e),
                        e.to_string(),
                    ));
                }

                // Step 4: rebuild the action with a uniform `inline`
                // source so the downstream `build_chromium_args` path
                // sees a single shape regardless of how the operator
                // framed the request.
                let resolved_source = serde_json::json!({
                    "source": "inline",
                    "cookies": cookies,
                });
                Action::WebSetCookies {
                    session_id,
                    source: resolved_source,
                }
            }
            // web.set_input_files: AUTHORITATIVE upload allow-list gate
            // (plan-council FND#5 — daemon-side, before the wasm guest /
            // chromium ever see the paths). Enforced in ALL profiles, fail
            // closed when LOOM_UPLOAD_ROOT is unset. On success the action is
            // rebuilt with CANONICALIZED paths so chromium opens the validated
            // file (closing most of the canonicalize→read TOCTOU window).
            Action::WebSetInputFiles {
                session_id,
                selector,
                paths,
            } => {
                match upload_guard::validate_upload_paths(
                    &paths,
                    self.upload_root.as_deref(),
                    upload_guard::MAX_UPLOAD_FILES,
                    upload_guard::MAX_UPLOAD_FILE_BYTES,
                    upload_guard::MAX_UPLOAD_TOTAL_BYTES,
                ) {
                    Ok(canon) => Action::WebSetInputFiles {
                        session_id,
                        selector,
                        paths: canon
                            .into_iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                    },
                    Err(e) => {
                        tracing::warn!(
                            session_id = %session_id_str,
                            kind = e.kind(),
                            "blocked file upload (upload allow-list)"
                        );
                        return Ok(upload_error_receipt(
                            session.allocate_action_id(),
                            session_id_str,
                            e.kind(),
                            e.message(),
                        ));
                    }
                }
            }
            other => other,
        };

        let handle = tokio::runtime::Handle::current();

        // web.network_log is a host/shim READ — it does NOT run the WASM guest,
        // navigate, or touch the replay hash chain. Intercept here and build the
        // receipt directly from the shim accumulator.
        if let Action::WebNetworkLog { .. } = &action {
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let action_id = session.allocate_action_id();
            // Plain block_on: we're on a spawn_blocking thread (see the
            // WasmHostBridge threading contract) — block_in_place would
            // panic here, and isn't needed off the worker pool.
            let data = handle.block_on(host.network_log(&sid)).map_err(|e| {
                tracing::error!(
                    session_id = %sid,
                    error = %e,
                    "web.network_log failed"
                );
                map_loom_error(&e)
            })?;
            return Ok(build_network_log_receipt(action_id, session_id_str, data));
        }

        // video-capture: web.start_recording / web.stop_recording are
        // host/shim-side streaming actions (CDP Page.startScreencast + an ffmpeg
        // encode) — they do NOT run the WASM guest, navigate, or touch the
        // replay hash chain. Intercept here, exactly like web.network_log. The
        // recorded .webm bytes live in CAS OUTSIDE the chain (only the content
        // hash is returned), so recording never affects replay-equality.
        if let Action::WebStartRecording {
            max_duration_ms,
            max_bytes,
            frame_rate,
            ..
        } = &action
        {
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let action_id = session.allocate_action_id();
            // Resolve optional caps to the safe defaults (plan D5).
            let dur = max_duration_ms.unwrap_or(300_000);
            let bytes = max_bytes.unwrap_or(268_435_456);
            let fps = frame_rate.map(|f| f.clamp(1, 60) as u32).unwrap_or(10);
            match handle.block_on(host.start_recording(&sid, dur, bytes, fps)) {
                Ok(()) => return Ok(build_recording_started_receipt(action_id, session_id_str)),
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "recording_start_failed",
                        e.to_string(),
                    ))
                }
            }
        }
        if let Action::WebStopRecording { .. } = &action {
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let action_id = session.allocate_action_id();
            match handle.block_on(host.stop_recording(&sid)) {
                Ok(result) => {
                    return Ok(build_stop_recording_receipt(
                        action_id,
                        session_id_str,
                        result,
                    ))
                }
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "recording_stop_failed",
                        e.to_string(),
                    ))
                }
            }
        }

        // voice-call-io (task 04): web.inject_audio is a host/shim side-channel —
        // it does NOT run the WASM guest, navigate, or enter the replay hash chain
        // (the receipt carries a CONSTANT outcome_hash). The daemon resolves the
        // payload (blob-ref → CAS / inline base64) and size-bounds it here so the
        // shim stays CAS-free (PRD D12); a real WebRTC call is inherently non-
        // deterministic, so audio verbs hard-error on a determinism-enabled session
        // BEFORE touching the shim — a paused virtual clock would freeze the call
        // (PRD D5).
        if let Action::WebInjectAudio {
            blob_ref,
            audio_b64,
            await_playout,
            ..
        } = &action
        {
            let action_id = session.allocate_action_id();

            // D5: audio requires no_determinism.
            if !session.no_determinism {
                return Ok(recording_error_receipt(
                    action_id,
                    session_id_str,
                    "determinism_enabled",
                    "web.inject_audio requires a no-determinism session (a paused \
                     virtual clock would freeze the live call); create the session \
                     with no-determinism enabled"
                        .to_string(),
                ));
            }

            // Resolve + size-bound the payload daemon-side (A5, D12).
            let bytes = match resolve_inject_payload(
                blob_ref.as_deref(),
                audio_b64.as_deref(),
                &self.core.content_store,
            ) {
                Ok(b) => b,
                Err((kind, message)) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        kind,
                        message,
                    ))
                }
            };

            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let await_playout = await_playout.unwrap_or(false);
            match handle.block_on(host.inject_audio(&sid, bytes, await_playout)) {
                Ok(outcome) => {
                    // D18 observability: the daemon logs enqueue/playout completion;
                    // it is deliberately NOT a receipt field (plan-council: no public
                    // schema creep) — the receipt carries only the constant hash.
                    tracing::info!(
                        session_id = %sid,
                        duration_ms = outcome.duration_ms,
                        awaited_playout = outcome.awaited_playout,
                        "audio.inject_enqueued"
                    );
                    return Ok(build_inject_audio_receipt(action_id, session_id_str));
                }
                Err(e) => {
                    let msg = e.to_string();
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        classify_inject_error(&msg),
                        msg,
                    ));
                }
            }
        }

        // voice-call-io (task 06): web.start_audio_capture / web.stop_audio_capture
        // are host/shim side-channels — no WASM guest, no navigate, no hash chain,
        // exactly like recording. The captured WAV is written to CAS host-side; the
        // stop receipt carries the observational `audio_after_hash` (which lives
        // OUTSIDE the manifest chain) plus a CONSTANT per-verb `outcome_hash`. Like
        // inject, capture hard-errors on a determinism-enabled session BEFORE
        // touching the shim — a paused virtual clock would freeze the live call
        // (PRD D5). The capture target is always the session's OWN live target
        // (resolved host-side via `shim_session_id_for`); the wire action carries no
        // caller-supplied `target_id`, so a cross-session target cannot be forged.
        if let Action::WebStartAudioCapture {
            max_duration_ms,
            max_bytes,
            ..
        } = &action
        {
            let action_id = session.allocate_action_id();
            if !session.no_determinism {
                return Ok(recording_error_receipt(
                    action_id,
                    session_id_str,
                    "determinism_enabled",
                    "web.start_audio_capture requires a no-determinism session (a paused \
                     virtual clock would freeze the live call); create the session \
                     with no-determinism enabled"
                        .to_string(),
                ));
            }
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            // Resolve optional caps to safe defaults; the shim re-clamps via
            // `Caps::sanitized`, so the public API can never disable the caps.
            let dur = max_duration_ms.unwrap_or(300_000);
            // Default byte cap sized NOT to truncate before the duration default:
            // 16 kHz mono i16 ≈ 32 KB/s, so 300 s ≈ 9.6 MiB — 16 MiB gives headroom
            // (still well under the 64 MiB host ceiling). Callers pass max_bytes for a
            // tighter bound. (#4: the previous 1 MiB default truncated at ~32 s.)
            let bytes = max_bytes.unwrap_or(16 * 1024 * 1024);
            match handle.block_on(host.start_audio_capture(&sid, dur, bytes)) {
                Ok(()) => {
                    return Ok(build_audio_capture_started_receipt(
                        action_id,
                        session_id_str,
                    ))
                }
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "audio_capture_start_failed",
                        e.to_string(),
                    ))
                }
            }
        }
        if let Action::WebStopAudioCapture { .. } = &action {
            let action_id = session.allocate_action_id();
            if !session.no_determinism {
                return Ok(recording_error_receipt(
                    action_id,
                    session_id_str,
                    "determinism_enabled",
                    "web.stop_audio_capture requires a no-determinism session (a paused \
                     virtual clock would freeze the live call); create the session \
                     with no-determinism enabled"
                        .to_string(),
                ));
            }
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            match handle.block_on(host.stop_audio_capture(&sid)) {
                Ok(result) => {
                    return Ok(build_stop_audio_capture_receipt(
                        action_id,
                        session_id_str,
                        result,
                    ))
                }
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "audio_capture_stop_failed",
                        e.to_string(),
                    ))
                }
            }
        }

        // voice-call-io (task 09): web.say is a host/shim side-channel that layers on
        // inject_audio — the daemon synthesizes `text` → audio bytes via the
        // operator-configured TTS backend (LOOM_TTS_CMD / LOOM_TTS_URL, hardened in
        // tts_backend.rs) and feeds them through the SAME inject path, so `say`
        // inherits every inject bound (size cap, replay exclusion). Like inject, it
        // hard-errors on a determinism-enabled session BEFORE synthesizing — a paused
        // virtual clock would freeze the live call (PRD D5). `text` is NEVER logged
        // (it can be sensitive call audio — plan-council P-A3); only its length and
        // the failure class are observable.
        if let Action::WebSay {
            text,
            await_playout,
            ..
        } = &action
        {
            let action_id = session.allocate_action_id();

            if !session.no_determinism {
                return Ok(recording_error_receipt(
                    action_id,
                    session_id_str,
                    "determinism_enabled",
                    "web.say requires a no-determinism session (a paused virtual clock \
                     would freeze the live call); create the session with no-determinism \
                     enabled"
                        .to_string(),
                ));
            }

            // Synthesize daemon-side (subprocess / SSRF-guarded HTTP). A typed TtsError
            // maps straight to the receipt error.kind; the Display message never echoes
            // `text` (P-A3).
            let text_len = text.len();
            let bytes = match handle.block_on(crate::tts_backend::synthesize(text)) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id_str,
                        kind = e.kind(),
                        text_len,
                        "audio.say_tts_failed"
                    );
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        e.kind(),
                        e.to_string(),
                    ));
                }
            };

            // A14: synthesized bytes pass the SAME inject size bound as any payload.
            if bytes.len() > MAX_INJECT_BYTES {
                return Ok(recording_error_receipt(
                    action_id,
                    session_id_str,
                    "payload_too_large",
                    format!(
                        "synthesized audio is {} bytes, over the {MAX_INJECT_BYTES}-byte cap",
                        bytes.len()
                    ),
                ));
            }

            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let await_playout = await_playout.unwrap_or(false);
            match handle.block_on(host.inject_audio(&sid, bytes, await_playout)) {
                Ok(outcome) => {
                    tracing::info!(
                        session_id = %sid,
                        text_len,
                        duration_ms = outcome.duration_ms,
                        awaited_playout = outcome.awaited_playout,
                        "audio.say_enqueued"
                    );
                    return Ok(build_say_receipt(action_id, session_id_str));
                }
                Err(e) => {
                    let msg = e.to_string();
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        classify_inject_error(&msg),
                        msg,
                    ));
                }
            }
        }

        // cdp-trusted-input: web.click is ALWAYS trusted — host-side CDP
        // `Input.dispatchMouseEvent` at the element hit point (like recording, no
        // guest). web.type `mode:keystrokes` and web.press_key are likewise
        // host-side `Input.dispatchKeyEvent` flows. Real input side effects
        // happen at record time only; the receipt's `outcome_hash` is a constant
        // marker so replay stays structural (NFR-DET-01).
        if let Action::WebClick {
            selector, until, ..
        } = &action
        {
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let sel = selector.clone();
            let settle_until = until.clone();
            let action_id = session.allocate_action_id();
            let dispatch_t0 = std::time::Instant::now();
            match handle.block_on(host.trusted_click(
                &sid,
                &sel,
                interaction_dispatch_budget_ms(deadline_ms),
            )) {
                Ok(outcome) => {
                    let dispatch_elapsed_ms = dispatch_t0.elapsed().as_millis() as u64;
                    let dispatched = matches!(
                        outcome,
                        loom_host::shim_manager::InputDispatchOutcome::Ok
                            | loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending
                    );
                    let mut receipt =
                        build_input_dispatch_receipt(action_id, session_id_str, &action, outcome);
                    if dispatched {
                        settle_after_input_dispatch(
                            &handle,
                            &host,
                            &session,
                            &sid,
                            settle_until.as_deref(),
                            deadline_ms,
                            dispatch_elapsed_ms,
                            &mut receipt,
                        );
                    }
                    return Ok(receipt);
                }
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "click_failed",
                        e.to_string(),
                    ))
                }
            }
        }
        if let Action::WebType {
            selector,
            text,
            mode,
            until,
            ..
        } = &action
        {
            // cdp-trusted-input: the default `fill` mode (CDP Input.insertText —
            // Playwright fill()) and `keystrokes` are dispatched host-side here;
            // `value` (and any unknown string) falls through to the WASM-guest
            // Runtime.evaluate path. Single source of truth: classify_web_type_mode.
            let dispatch = classify_web_type_mode(mode.as_deref());
            if dispatch != WebTypeDispatch::ValueGuest {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let sel = selector.clone();
                let txt = text.clone();
                let settle_until = until.clone();
                let action_id = session.allocate_action_id();
                let dispatch_t0 = std::time::Instant::now();
                let dispatch_budget = interaction_dispatch_budget_ms(deadline_ms);
                let outcome = match dispatch {
                    WebTypeDispatch::Fill => {
                        handle.block_on(host.type_fill(&sid, &sel, &txt, dispatch_budget))
                    }
                    WebTypeDispatch::Keystrokes => {
                        handle.block_on(host.type_keystrokes(&sid, &sel, &txt, dispatch_budget))
                    }
                    WebTypeDispatch::ValueGuest => unreachable!("guarded above"),
                };
                match outcome {
                    Ok(outcome) => {
                        let dispatch_elapsed_ms = dispatch_t0.elapsed().as_millis() as u64;
                        let dispatched = matches!(
                            outcome,
                            loom_host::shim_manager::InputDispatchOutcome::Ok
                                | loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending
                        );
                        let mut receipt = build_input_dispatch_receipt(
                            action_id,
                            session_id_str,
                            &action,
                            outcome,
                        );
                        if dispatched {
                            settle_after_input_dispatch(
                                &handle,
                                &host,
                                &session,
                                &sid,
                                settle_until.as_deref(),
                                deadline_ms,
                                dispatch_elapsed_ms,
                                &mut receipt,
                            );
                        }
                        return Ok(receipt);
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "type_failed",
                            e.to_string(),
                        ))
                    }
                }
            }
        }
        if let Action::WebPressKey {
            key,
            selector,
            modifiers,
            ..
        } = &action
        {
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let k = key.clone();
            let sel = selector.clone();
            let mods = modifiers.clone().unwrap_or_default();
            let action_id = session.allocate_action_id();
            match handle.block_on(host.press_key(
                &sid,
                &k,
                sel,
                mods,
                interaction_dispatch_budget_ms(deadline_ms),
            )) {
                Ok(outcome) => {
                    return Ok(build_input_dispatch_receipt(
                        action_id,
                        session_id_str,
                        &action,
                        outcome,
                    ))
                }
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "press_key_failed",
                        e.to_string(),
                    ))
                }
            }
        }

        // web.wait is intercepted host-side (like web.click) so it can resolve the
        // SAME `>>` locator grammar (`text=`/`role=`/`css=`/`frame=`) via
        // `host.wait` → `send_wait` and poll until the locator resolves or
        // `timeout_ms` elapses — instead of the old guest path that passed the raw
        // value to `querySelector` (which threw on `text=`/`role=`). Poll timing is
        // never recorded; only the Resolved/PredicateFalse verdict, so replay stays
        // structural (NFR-DET-01).
        if let Action::WebWait {
            selector,
            timeout_ms,
            ..
        } = &action
        {
            let host = Arc::clone(&self.host);
            let sid = session_id_str.to_string();
            let sel = selector.clone();
            let to = *timeout_ms;
            let action_id = session.allocate_action_id();
            match handle.block_on(host.wait(&sid, &sel, to)) {
                Ok(outcome) => {
                    return Ok(build_wait_receipt(
                        action_id,
                        session_id_str,
                        &action,
                        outcome,
                    ))
                }
                Err(e) => {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "wait_failed",
                        e.to_string(),
                    ))
                }
            }
        }

        let session_handle = SessionHandle {
            session_id: session.id.clone(),
            handle: handle.clone(),
            receipt_pool: handle.clone(),
            abort_flag: session.abort_flag.clone(),
            abort_signal: session.abort_notify.clone(),
            kill_reason: session.kill_reason.clone(),
            seed: session.seed,
            // The session's OWN harness — per-session RNG/clock isolation
            // (never the facade singleton).
            determinism: session.determinism.clone(),
            epoch_ms: session.epoch_ms,
            no_blocklist: session.no_blocklist,
            // settle-capture (4b): thread the determinism toggle through.
            no_determinism: session.no_determinism,
            // voice-call-io: thread the --audio opt-in through to HostState so
            // target-creating host fns carry audio_enabled on the shim wire.
            audio: session.audio,
            // thread profile + downloads_dir into the
            // SessionHandle so HostState can inject env vars at shim spawn.
            profile: session.profile.clone(),
            downloads_dir: session.downloads_dir.clone(),
            // capture-policy=fingerprint tier: derived ONCE here from the
            // authoritative session capture_policy. Gates the post-action DOM
            // fingerprint (host fn + decode accept-gate). Other policies → false
            // → no extra DOM.getDocument round-trip, byte-identical receipts.
            capture_dom_after: session.capture_policy.as_deref() == Some("fingerprint"),
        };

        // Build the host-side action payload. Three shapes flow through
        // here:
        //
        //   1. `web.navigate` — the WIT guest's `navigate-verb` calls
        //      `host::navigate_execute(&url, deadline_ms)` directly; its
        //      `Action.payload` MUST be raw UTF-8 URL bytes (per the
        //      contract documented at `loom-surface-web::navigate_verb`).
        //      `build_chromium_args` produces a CBOR-shaped CdpMessage,
        //      which `String::from_utf8` rejects — so navigate skips
        //      that path.
        //   2. Other web verbs (click/type/etc.) — route through the
        //      generic `host::shim_call("chromium", &a.payload)` path,
        //      where the chromium shim expects a CBOR-encoded CdpMessage.
        //      `build_chromium_args` produces exactly that.
        //   3. Anything else — fall back to JCS-encoded Action.
        let args_canonical_bytes = match &action {
            // Navigate AND evaluate use the typed host functions
            // (`navigate_execute` / `evaluate_execute`) — both expect
            // raw UTF-8 bytes in the action payload, NOT a CBOR-encoded
            // CdpMessage. Mismatch returns `HostError::Internal("payload
            // not valid UTF-8 ...")` from the guest, which surfaces as
            // `internal_error: action dispatch failed` to the CLI.
            // (This regression was caught after the evaluate-result-not-surfaced
            // feature landed.)
            // settle-capture: the navigate payload is `<until>\n<url>` so the
            // readiness mode rides to the guest (which has no JSON parser and
            // splits once on the newline). URLs are http/https/about:blank —
            // never contain a newline — so the split is unambiguous.
            Action::WebNavigate { url, until, .. } => {
                let mode = until.as_deref().unwrap_or("settled");
                format!("{mode}\n{url}").into_bytes()
            }
            Action::WebEvaluate { expression, .. } => expression.as_bytes().to_vec(),
            // web.scroll reuses the evaluate tier: the daemon emits the scroll JS
            // as a RAW expression (not a CBOR CdpMessage), so the guest's
            // `scroll_verb` runs it via `evaluate_execute` and surfaces the
            // post-scroll viewport position. action_hash = sha256(this expression)
            // — covers selector + deltas + the viewport-target logic. Mirrors the
            // WebEvaluate arm above. (Determinism: the JS string is a pure function
            // of the inputs; replay copies recorded receipt bytes, so old scroll
            // recordings still validate against their own bytes.)
            Action::WebScroll {
                selector,
                delta_x,
                delta_y,
                ..
            } => build_scroll_expression(selector, delta_x.unwrap_or(0), delta_y.unwrap_or(0))
                .into_bytes(),
            // settle-capture: web.wait_for's guest verb (`wait_for_verb`) reads
            // the payload as the readiness mode string and calls the typed
            // `host::wait_for_execute`. Raw UTF-8 `until` bytes (default
            // `settled`) — never a CBOR CdpMessage.
            Action::WebWaitFor { until, .. } => {
                until.as_deref().unwrap_or("settled").as_bytes().to_vec()
            }
            // web.set_input_files: the guest's `set_input_files_verb` decodes
            // {selector, paths} from the payload and calls
            // `host::set_input_files_execute`. Paths here are already
            // canonicalized by the upload gate above. action_hash =
            // sha256(payload) covers selector + canonical paths.
            Action::WebSetInputFiles {
                selector, paths, ..
            } => serde_json::to_vec(&serde_json::json!({
                "selector": selector,
                "paths": paths,
            }))
            .unwrap_or_default(),
            _ => build_chromium_args(&action).unwrap_or_else(|| {
                serde_jcs::to_string(&action)
                    .unwrap_or_default()
                    .into_bytes()
            }),
        };
        // per-session monotonic action_id, allocated at
        // dispatch time. The same id is plumbed through HostState →
        // ReceiptBuilder → ActionReceipt (WAL) → Receipt (RPC reply), so the
        // value the CLI sees matches `loom session inspect` entries[].action_id.
        let host_action = HostAction {
            action_id: session.allocate_action_id(),
            surface: action_surface(&action).to_string(),
            method: action_verb(&action).to_string(),
            args_canonical_bytes,
            // Per-action kill deadline (dispatch metadata; excluded from the
            // hash chain — see Action::deadline_ms). The executor races it
            // against the guest call and traps `request_timeout` on expiry.
            deadline_ms,
        };

        let host = Arc::clone(&self.host);
        // Plain block_on: we're on a spawn_blocking thread (see the
        // WasmHostBridge threading contract) — block_in_place would
        // panic here, and isn't needed off the worker pool.
        let outcome = handle
            .block_on(host.dispatch(host_action, session_handle))
            .map_err(|e| {
                // Surface-side dispatch failed at the wasmtime / IPC layer.
                // Don't drop the error message — it's our only signal for
                // diagnosing why navigate / click / evaluate trapped (e.g.
                // shim crash, WIT signature mismatch, OOM in the guest).
                // The ERROR-level log fires regardless of RUST_LOG since the
                // subscriber's default fallback is `warn`. The wire kind
                // stays `SurfaceTrap` so existing CLI / receipt schemas
                // don't churn.
                tracing::error!(
                    surface = %action_surface(&action),
                    method = %action_verb(&action),
                    error = %e,
                    "host.dispatch failed → SurfaceTrap"
                );
                LoomErrorCode::SurfaceTrap
            })?;

        // video-capture: whole-session auto-start. After a successful navigate
        // (a live page now exists) begin recording if the session opted in via
        // `--record-screencast`. Idempotent — the recorder rejects an
        // already-active start, so later navigates are harmless no-ops and the
        // recording spans the whole session until `shutdown_session` finalizes
        // it (→ recordings.jsonl sidecar). Best-effort: a start failure is
        // logged at debug and never affects the navigate receipt.
        if session.record_screencast
            && matches!(&action, Action::WebNavigate { .. })
            && matches!(&outcome, ActionOutcome::Success { .. })
        {
            if let Err(e) =
                handle.block_on(
                    self.host
                        .start_recording(session_id_str, 300_000, 268_435_456, 10),
                )
            {
                tracing::debug!(
                    session_id = %session_id_str,
                    error = %e,
                    "whole-session screencast auto-start skipped (likely already recording)"
                );
            }
        }

        match outcome {
            ActionOutcome::Success { builder, .. } => {
                let mut receipt = build_navigate_wire_receipt(
                    &builder,
                    session_id_str,
                    session.capture_policy.as_deref(),
                );
                // web.scroll surfaces its post-scroll viewport position through the
                // evaluate tier (return_value_json). Promote it into the purpose-named
                // `scroll_result` field (single source of truth for scroll). Follows
                // the capture policy already applied to return_value_json (both
                // stripped together under `minimal`).
                if matches!(action, Action::WebScroll { .. }) {
                    promote_scroll_result(&mut receipt);
                }
                Ok(receipt)
            }
            ActionOutcome::Aborted { .. } => Err(LoomErrorCode::SessionAborted),
            ActionOutcome::Trapped { loom_error, .. } => {
                // Propagate the typed code the host already mapped.
                // `decode_typed_receipt` translates the WIT
                // `host-error` variant (`shim-failure`,
                // `budget-exceeded`, etc.) into the matching
                // `loom_core::LoomErrorCode`; for genuine wasmtime
                // traps `trap_handler::handle_trap` produces
                // SurfaceTrap. Either way, route through
                // `map_loom_error` so the rpc layer sees the right
                // code instead of a hardcoded SurfaceTrap.
                Err(map_loom_error(&loom_error))
            }
        }
    }

    fn has_chromium(&self) -> bool {
        self.has_chromium
    }
}

/// Build a WasmHostBridge. Tries to create a real `WasmHost`; if the
/// surfaces directory is missing or modules haven't been compiled yet,
/// falls back to the stub that returns `SurfaceUnavailable`.
///
/// Returns both the bridge (for the host service adapter) and the
/// underlying `Arc<WasmHost>` (for the CoreBridge so session-close can
/// trigger shim teardown). When a real WasmHost couldn't be built, the
/// second slot is None.
pub(crate) fn build_host_bridge(
    core: Arc<CoreApiFacade>,
    upload_root: Option<PathBuf>,
) -> (Arc<dyn WasmHostBridge>, Option<Arc<loom_host::WasmHost>>) {
    use loom_host::{HostConfig, ShimChromiumConfig, WasmHost};

    // Surface the upload-root posture at startup so operators aren't left
    // guessing why web.set_input_files fails closed (it denies all uploads
    // when LOOM_UPLOAD_ROOT is unset).
    match &upload_root {
        Some(root) => tracing::info!(
            upload_root = %root.display(),
            "web.set_input_files uploads enabled (LOOM_UPLOAD_ROOT)"
        ),
        None => tracing::info!(
            "web.set_input_files uploads DISABLED — set LOOM_UPLOAD_ROOT to enable (fail-closed)"
        ),
    }

    // Resolve surfaces dir the same way `loom postinstall` writes them
    // (~/.config/loom/surfaces/) so AOT-compiled .cwasm modules are found.
    // The CLI's compiled_defaults() hardcodes `home.join(".config").join("loom")`
    // (cli_config/interfaces.rs), so the daemon MUST mirror that path verbatim.
    // dirs::config_dir() returns ~/Library/Application Support on macOS — wrong.
    let surfaces_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("loom")
        .join("surfaces");

    // resolve Chromium across all install channels. Resolution
    // chain: LOOM_CHROMIUM_PATH env override → pinned `~/.config/loom/chromium/...`
    // → PATH search (chromium / chromium-browser / chrome / google-chrome) →
    // macOS `/Applications/...` → typed `BrowserNotFound`. Pinned wins for
    // replay-bit-equality ; a `tracing::warn!` fires below when
    // the resolver picks a non-pinned source so users know they've lost
    // determinism.
    let chromium_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("loom")
        .join("chromium");
    let shim_chromium = match loom_shared::chromium_resolver::resolve_chromium(&chromium_dir) {
        Ok((chromium, source)) => {
            use loom_shared::chromium_resolver::ChromiumSource;
            if source != ChromiumSource::Pinned {
                tracing::warn!(
                    chromium_path = %chromium.display(),
                    source = ?source,
                    "loom: using system-installed Chromium; replay-bit-equality is \
                     not guaranteed across machines. Run 'loom postinstall' for the \
                     pinned build."
                );
            }
            loom_shared::binary_resolver::resolve_loom_sibling("loom-shim-chromium").map(
                |shim_bin| ShimChromiumConfig {
                    shim_binary_path: shim_bin,
                    chromium_path: chromium,
                },
            )
        }
        Err(not_found) => {
            tracing::error!(
                searched = ?not_found.searched_paths,
                "Chromium not found by any resolver path. \
                 Install via 'brew install --cask chromium' (macOS) or your \
                 distro's package manager (Linux), or run 'loom postinstall' \
                 for the pinned build. session.create will return BrowserNotFound \
                 until Chromium is reachable."
            );
            None
        }
    };

    let has_chromium = shim_chromium.is_some();
    let host_config = HostConfig {
        surfaces_dir,
        shim_chromium,
        ..HostConfig::default()
    };

    match WasmHost::new(Arc::clone(&core), host_config) {
        Ok(host) => {
            let host_for_bridge = Arc::clone(&host);
            (
                Arc::new(WasmBridge {
                    host: host_for_bridge,
                    core,
                    has_chromium,
                    upload_root,
                }),
                Some(host),
            )
        }
        Err(_) => {
            tracing::warn!(
                "WasmHost unavailable — run `loom postinstall` to compile surface modules"
            );
            (Arc::new(StubHostBridge), None)
        }
    }
}

/// Maximum decoded inject payload — 8 MiB (PRD FND-0029). A larger clip is
/// rejected; the bound also caps in-page decode/playout CPU.
pub(crate) const MAX_INJECT_BYTES: usize = 8 * 1024 * 1024;

/// Resolve + size-bound a `web.inject_audio` payload daemon-side (Architecture §3
/// "Inject", A5, PRD D12). Exactly one of `blob_ref` / `audio_b64` must be present.
/// Returns the resolved, size-checked bytes, or `(typed_kind, message)` on
/// rejection (mapped to a typed error receipt by the caller). The shim never sees
/// the CAS — it receives raw bytes.
fn resolve_inject_payload(
    blob_ref: Option<&str>,
    audio_b64: Option<&str>,
    content_store: &std::sync::Arc<dyn loom_core::content_store::ContentStore>,
) -> Result<Vec<u8>, (&'static str, String)> {
    use base64::Engine as _;
    match (blob_ref, audio_b64) {
        (Some(_), Some(_)) => Err((
            "invalid_argument",
            "web.inject_audio takes exactly one of blob_ref or audio_b64, not both".to_string(),
        )),
        (None, None) => Err((
            "invalid_argument",
            "web.inject_audio requires either blob_ref or audio_b64".to_string(),
        )),
        (Some(sha), None) => {
            // Validate the ref is a real content hash BEFORE using it as a CAS
            // path component (defense-in-depth vs a traversal-shaped blob_ref).
            if sha.len() != 64
                || !sha
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err((
                    "invalid_argument",
                    "blob_ref must be a 64-char lowercase hex sha256".to_string(),
                ));
            }
            let cref = loom_core::content_store::ContentRef {
                sha256: sha.to_string(),
                size_bytes: 0, // get() re-hashes by path; size_bytes is not validated.
            };
            let bytes = content_store.get(&cref).map_err(|e| {
                (
                    "blob_not_found",
                    format!("blob_ref {sha} not resolvable: {e}"),
                )
            })?;
            if bytes.len() > MAX_INJECT_BYTES {
                return Err((
                    "payload_too_large",
                    format!(
                        "resolved audio is {} bytes, over the {MAX_INJECT_BYTES}-byte cap",
                        bytes.len()
                    ),
                ));
            }
            Ok(bytes)
        }
        (None, Some(b64)) => {
            // A5: reject BEFORE decode when the encoded length cannot fit within
            // the decoded cap — bounds decode amplification / inline-base64 DoS.
            // base64 encodes 3 bytes → 4 chars, so max encoded len = ceil(N/3)*4.
            let encoded_cap = MAX_INJECT_BYTES.div_ceil(3) * 4;
            if b64.len() > encoded_cap {
                return Err((
                    "payload_too_large",
                    format!(
                        "base64 payload is {} chars, over the {encoded_cap}-char cap \
                         (decoded max {MAX_INJECT_BYTES} bytes)",
                        b64.len()
                    ),
                ));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|_| {
                    (
                        "audio_decode_failed",
                        "audio_b64 is not valid base64".to_string(),
                    )
                })?;
            if bytes.len() > MAX_INJECT_BYTES {
                return Err((
                    "payload_too_large",
                    format!(
                        "decoded audio is {} bytes, over the {MAX_INJECT_BYTES}-byte cap",
                        bytes.len()
                    ),
                ));
            }
            Ok(bytes)
        }
    }
}

#[cfg(test)]
mod settle_budget_tests {
    use super::*;

    // Pure-function tests (no env): the base + server_cap are passed in, so these
    // are deterministic and race-free (no LOOM_* env mutation, no ENV_LOCK needed).
    const BASE: u64 = DEFAULT_SETTLE_BASE_MS; // 10_000
    const CAP: u64 = 30_000; // illustrative server cap (loom-rpc default)
    const H: u64 = SETTLE_HEADROOM_MS; // 3_000

    #[test]
    fn no_deadline_uses_base_when_under_cap() {
        // None / Some(0) ⇒ no caller deadline ⇒ base (base << cap - headroom).
        assert_eq!(clamp_settle_budget(BASE, CAP, None, 0), BASE);
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(0), 0), BASE);
    }

    #[test]
    fn caller_deadline_clamps_to_remaining_minus_headroom() {
        // deadline 5s, 200ms elapsed ⇒ 5000-200-3000 = 1800, under base.
        assert_eq!(
            clamp_settle_budget(BASE, CAP, Some(5_000), 200),
            5_000 - 200 - H
        );
    }

    #[test]
    fn deadline_larger_than_server_cap_is_clamped_to_cap() {
        // THE REPORTED CASE: caller sends 45s, server cap 30s. The effective
        // deadline is the 30s cap, so budget is base (base < cap-headroom), never
        // 45s — else the RPC race beats the settle → transport request_timeout.
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(45_000), 0), BASE);
        // With a large base (LOOM_SHIM_CDP_TIMEOUT_MS raised past the cap) the
        // server cap - headroom bounds it.
        assert_eq!(clamp_settle_budget(50_000, CAP, Some(45_000), 0), CAP - H);
        assert_eq!(clamp_settle_budget(50_000, CAP, None, 0), CAP - H);
    }

    #[test]
    fn tight_caller_deadline_bounds_budget_not_just_server_cap() {
        // FND-0009: a caller deadline TIGHTER than the server cap must bound the
        // budget (and there is no floor to push it back up). deadline 2s, 300ms
        // elapsed ⇒ 2000-300-3000 saturates to 0 — out of time, budget 0, NOT
        // floored up to a "minimum" that would overrun the 2s caller deadline.
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(2_000), 300), 0);
        // deadline 4s, 0 elapsed ⇒ 4000-3000 = 1000, well under the server cap.
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(4_000), 0), 4_000 - H);
    }

    #[test]
    fn no_floor_up_near_deadline() {
        // FND-0009: near the deadline the budget shrinks to 0 rather than being
        // floored UP (which is what re-tripped request_timeout). No lower bound.
        assert_eq!(clamp_settle_budget(BASE, CAP, None, 27_500), 0); // 30000-27500-3000 sat 0
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(45_000), 29_400), 0);
        // A hair of headroom yields a correspondingly tiny (sub-500) budget, honestly.
        assert_eq!(
            clamp_settle_budget(BASE, CAP, None, 26_800),
            30_000 - 26_800 - H
        ); // 200
    }

    #[test]
    fn saturating_math_no_underflow_past_cap() {
        // elapsed > effective deadline must saturate to 0, never panic (FND-0006).
        assert_eq!(clamp_settle_budget(BASE, CAP, None, 40_000), 0);
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(45_000), 40_000), 0);
        assert_eq!(clamp_settle_budget(BASE, 0, None, 0), 0); // degenerate zero cap
    }

    // (The headroom-covers-resume invariant is enforced at compile time by the
    // `const _: () = assert!(SETTLE_HEADROOM_MS >= 2_000)` guard at the constant's
    // definition — a runtime test would be a clippy `assertions_on_constants`.)
}

#[cfg(test)]
mod inject_payload_tests {
    use super::*;
    use base64::Engine as _;
    use loom_core::content_store::{ContentRef, ContentStore, GcReport};
    use loom_core::error::{LoomError, LoomErrorCode};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Minimal in-module ContentStore double (loom_core::mocks is feature-gated
    /// and not reachable from loom-daemon tests without extra Cargo wiring).
    struct MockStore(Mutex<HashMap<String, Vec<u8>>>);
    impl ContentStore for MockStore {
        fn put(&self, bytes: &[u8]) -> Result<ContentRef, LoomError> {
            let sha = loom_core::content_store::sha256_hex(bytes);
            self.0.lock().unwrap().insert(sha.clone(), bytes.to_vec());
            Ok(ContentRef {
                sha256: sha,
                size_bytes: bytes.len() as u64,
            })
        }
        fn get(&self, r: &ContentRef) -> Result<Vec<u8>, LoomError> {
            self.0
                .lock()
                .unwrap()
                .get(&r.sha256)
                .cloned()
                .ok_or_else(|| LoomError::new(LoomErrorCode::StoreNotFound, "not found"))
        }
        fn gc(&self, _ttl: Duration) -> Result<GcReport, LoomError> {
            Ok(GcReport {
                blobs_scanned: 0,
                blobs_collected: 0,
                bytes_freed: 0,
            })
        }
    }

    fn store() -> Arc<dyn ContentStore> {
        Arc::new(MockStore(Mutex::new(HashMap::new())))
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn valid_inline_base64_resolves_to_exact_bytes() {
        let s = store();
        let audio = b"RIFF----WAVEfake";
        let out = resolve_inject_payload(None, Some(&b64(audio)), &s).expect("resolve ok");
        assert_eq!(out, audio);
    }

    #[test]
    fn corrupt_base64_is_audio_decode_failed() {
        let s = store();
        let (kind, _) = resolve_inject_payload(None, Some("!!!not base64!!!"), &s).unwrap_err();
        assert_eq!(kind, "audio_decode_failed");
    }

    #[test]
    fn oversize_base64_rejected_before_decode() {
        let s = store();
        // A string longer than the encoded cap must be rejected WITHOUT decoding.
        let encoded_cap = MAX_INJECT_BYTES.div_ceil(3) * 4;
        let huge = "A".repeat(encoded_cap + 1);
        let (kind, _) = resolve_inject_payload(None, Some(&huge), &s).unwrap_err();
        assert_eq!(kind, "payload_too_large");
    }

    #[test]
    fn oversize_decoded_payload_rejected() {
        let s = store();
        // Encoded length within the cap, decoded length over 8 MiB.
        let big = vec![0u8; MAX_INJECT_BYTES + 1_000];
        let (kind, _) = resolve_inject_payload(None, Some(&b64(&big)), &s).unwrap_err();
        assert_eq!(kind, "payload_too_large");
    }

    #[test]
    fn neither_payload_is_invalid_argument() {
        let s = store();
        let (kind, _) = resolve_inject_payload(None, None, &s).unwrap_err();
        assert_eq!(kind, "invalid_argument");
    }

    #[test]
    fn both_payloads_is_invalid_argument() {
        let s = store();
        let (kind, _) = resolve_inject_payload(Some("a".repeat(64).as_str()), Some(&b64(b"x")), &s)
            .unwrap_err();
        assert_eq!(kind, "invalid_argument");
    }

    #[test]
    fn blob_ref_resolves_from_cas() {
        let s = store();
        let audio = b"WAVE-from-cas".to_vec();
        let cref = s.put(&audio).unwrap();
        let out = resolve_inject_payload(Some(&cref.sha256), None, &s).expect("resolve ok");
        assert_eq!(out, audio);
    }

    #[test]
    fn missing_blob_is_blob_not_found() {
        let s = store();
        let sha = "0".repeat(64);
        let (kind, _) = resolve_inject_payload(Some(&sha), None, &s).unwrap_err();
        assert_eq!(kind, "blob_not_found");
    }

    #[test]
    fn malformed_blob_ref_is_invalid_argument() {
        let s = store();
        let (kind, _) = resolve_inject_payload(Some("../../etc/passwd"), None, &s).unwrap_err();
        assert_eq!(kind, "invalid_argument");
    }
}
