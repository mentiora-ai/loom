//! Wire/receipt builders for the daemon's `WasmBridge` dispatch path.
//!
//! These are pure, leaf helper free functions split out of `lib.rs`
//! (large-file refactor): they synthesize `loom-rpc` wire `Receipt`s for
//! daemon-side gates (profile-restricted evaluate, upload/cookie/recording
//! errors), build CDP/CBOR or raw-JS action payloads (`build_chromium_args`,
//! `build_scroll_expression`), translate the host `ReceiptBuilder` into the
//! wire receipt (`build_navigate_wire_receipt` / `build_wire_receipt_error`),
//! and read the surface/verb/session-id off a typed `Action`. No behavior
//! change — moved verbatim.

use loom_rpc::host_service_adapter::host_service_adapter::{Action, Receipt};

/// synthesize an error Receipt for a safe-profile
/// evaluate that matched the denylist. Daemon-layer gate runs BEFORE
/// host.dispatch, so we never touch the shim. The wire shape:
///
/// ```text
/// {
///   "status": "error",
///   "error": {
///     "kind": "profile_restricted",
///     "detail": {
///       "matched_pattern": "<pattern>",
///       "profile": "safe",
///       "violation": "safe_profile_evaluate_denylist_match"
///     }
///   }
/// }
/// ```
///
/// `action_id` comes from `session.allocate_action_id()` so the rejection
/// counts against the per-session monotonic sequence .
pub(crate) fn profile_restricted_evaluate_receipt(
    action_id: u64,
    session_id: &str,
    matched_pattern: &str,
) -> Receipt {
    use loom_rpc::host_service_adapter::host_service_adapter::{ReceiptError, ReceiptStatus};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Receipt {
        action_id,
        session_id: session_id.to_string(),
        status: ReceiptStatus::Error,
        timing_ticks: 0,
        side_effects: vec![],
        error: Some(ReceiptError {
            kind: "profile_restricted".to_string(),
            detail: Some(serde_json::json!({
                "matched_pattern": matched_pattern,
                "profile": "safe",
                "violation": "safe_profile_evaluate_denylist_match",
            })),
        }),
        action_hash: None,
        outcome_hash: None,
        emitted_at_ms: Some(now),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        dom_after_hash: None,
        screenshot_after_hash: None,
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: None,
        network_count: None,
        console_lines: vec![],
        network_summary: None,
        network_entries: vec![],
        network_entries_blob_ref: None,
        network_entries_truncated: None,
        settle_until: None,
        settle_outcome: None,
        return_value_json: None,
        return_value_blob_ref: None,
        // v0.9.6 cookie-result fields — not applicable to a
        // profile-restricted evaluate.
        set_cookies_result: None,
        get_cookies_result: None,
        clear_cookies_result: None,
        delete_cookies_result: None,
        scroll_result: None,
    }
}

/// v0.9.7 follow-up: build an error Receipt for a per-cookie validation
/// rejection in `web.set_cookies`. The typed `CookieValidationError`
/// taxonomy is surfaced on the receipt's `error.kind` ("cookie_validation_error")
/// and `error.detail.code` (one of `name_empty` / `name_invalid` /
/// `value_too_large` / `too_many_cookies` / `invalid_expires`). The daemon
/// gate is the authoritative emitter under the daemon-owns-verbs architecture
/// (the retired `loom-surfaces` verb-side error mapper is gone).
/// Typed error receipt for `web.set_input_files` allow-list / cap rejections.
/// `kind` is the discrete `UploadError::kind()` wire string (e.g.
/// `upload_path_blocked`) — NOT a `js_throw` JSON blob (plan-council FND#8).
/// `message` uses basenames, not full host paths (L1).
pub(crate) fn upload_error_receipt(
    action_id: u64,
    session_id: &str,
    kind: &str,
    message: String,
) -> Receipt {
    use loom_rpc::host_service_adapter::host_service_adapter::{ReceiptError, ReceiptStatus};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Receipt {
        action_id,
        session_id: session_id.to_string(),
        status: ReceiptStatus::Error,
        timing_ticks: 0,
        side_effects: vec![],
        error: Some(ReceiptError {
            kind: kind.to_string(),
            detail: Some(serde_json::json!({ "message": message })),
        }),
        action_hash: None,
        outcome_hash: None,
        emitted_at_ms: Some(now),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        dom_after_hash: None,
        screenshot_after_hash: None,
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: None,
        network_count: None,
        console_lines: vec![],
        network_summary: None,
        network_entries: vec![],
        network_entries_blob_ref: None,
        network_entries_truncated: None,
        return_value_json: None,
        return_value_blob_ref: None,
        set_cookies_result: None,
        get_cookies_result: None,
        clear_cookies_result: None,
        delete_cookies_result: None,
        settle_until: None,
        settle_outcome: None,
        scroll_result: None,
    }
}

/// Synthesize the `loom.web.network_log` receipt from the host's read of the
/// shim accumulator. Observation-only: no navigate-tier fields, no hash chain.
pub(crate) fn build_network_log_receipt(
    action_id: u64,
    session_id: &str,
    data: loom_host::wasm_host::wasm_host::NetworkLogData,
) -> Receipt {
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Receipt {
        action_id,
        session_id: session_id.to_string(),
        status: ReceiptStatus::Success,
        timing_ticks: 0,
        side_effects: vec![],
        error: None,
        action_hash: None,
        outcome_hash: None,
        emitted_at_ms: Some(now),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        dom_after_hash: None,
        screenshot_after_hash: None,
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: None,
        network_count: None,
        console_lines: vec![],
        network_summary: None,
        network_entries: data.network_entries,
        network_entries_blob_ref: data.network_entries_blob_ref,
        network_entries_truncated: Some(data.network_entries_truncated),
        settle_until: None,
        settle_outcome: None,
        return_value_json: None,
        return_value_blob_ref: None,
        set_cookies_result: None,
        get_cookies_result: None,
        clear_cookies_result: None,
        delete_cookies_result: None,
        scroll_result: None,
    }
}

/// video-capture: success receipt for `web.start_recording` (the recording
/// began; the video hash arrives on the `web.stop_recording` receipt).
pub(crate) fn build_recording_started_receipt(action_id: u64, session_id: &str) -> Receipt {
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Receipt {
        action_id,
        session_id: session_id.to_string(),
        status: ReceiptStatus::Success,
        timing_ticks: 0,
        side_effects: vec![],
        error: None,
        action_hash: None,
        outcome_hash: None,
        emitted_at_ms: Some(now),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        dom_after_hash: None,
        screenshot_after_hash: None,
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: None,
        network_count: None,
        console_lines: vec![],
        network_summary: None,
        network_entries: vec![],
        network_entries_blob_ref: None,
        network_entries_truncated: None,
        settle_until: None,
        settle_outcome: None,
        return_value_json: None,
        return_value_blob_ref: None,
        set_cookies_result: None,
        get_cookies_result: None,
        clear_cookies_result: None,
        delete_cookies_result: None,
        scroll_result: None,
    }
}

/// video-capture: `web.stop_recording` receipt. On success carries
/// `screencast_after_hash` (the `.webm` CAS hash). A best-effort encode failure
/// (encoder unavailable / zero frames) → an error receipt whose detail carries
/// the `stop_reason` + `error` so the agent gets an actionable message; the
/// session itself was never aborted.
pub(crate) fn build_stop_recording_receipt(
    action_id: u64,
    session_id: &str,
    result: loom_host::wasm_host::wasm_host::ScreencastResult,
) -> Receipt {
    match result.screencast_after_hash {
        Some(hash) => {
            let mut r = build_recording_started_receipt(action_id, session_id);
            r.screencast_after_hash = Some(hash);
            r
        }
        None => recording_error_receipt(
            action_id,
            session_id,
            "recording_failed",
            result
                .error
                .unwrap_or_else(|| format!("recording produced no video ({})", result.stop_reason)),
        ),
    }
}

/// video-capture: error receipt for a recording start/stop failure. Best-effort
/// — recording never aborts the session, so this is an `Error`-status receipt,
/// not a session kill.
pub(crate) fn recording_error_receipt(
    action_id: u64,
    session_id: &str,
    kind: &str,
    message: String,
) -> Receipt {
    use loom_rpc::host_service_adapter::host_service_adapter::{ReceiptError, ReceiptStatus};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Receipt {
        action_id,
        session_id: session_id.to_string(),
        status: ReceiptStatus::Error,
        timing_ticks: 0,
        side_effects: vec![],
        error: Some(ReceiptError {
            kind: kind.to_string(),
            detail: Some(serde_json::json!({ "message": message })),
        }),
        action_hash: None,
        outcome_hash: None,
        emitted_at_ms: Some(now),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        dom_after_hash: None,
        screenshot_after_hash: None,
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: None,
        network_count: None,
        console_lines: vec![],
        network_summary: None,
        network_entries: vec![],
        network_entries_blob_ref: None,
        network_entries_truncated: None,
        settle_until: None,
        settle_outcome: None,
        return_value_json: None,
        return_value_blob_ref: None,
        set_cookies_result: None,
        get_cookies_result: None,
        clear_cookies_result: None,
        delete_cookies_result: None,
        scroll_result: None,
    }
}

/// voice-call-io (task 04): success receipt for `web.inject_audio`. Carries a
/// CONSTANT per-verb `outcome_hash` dispatch-success marker (NOT audio/page state),
/// exactly like the interaction verbs (`build_input_dispatch_receipt`) and
/// `build_wait_receipt`, so a voice session's manifest hash chain stays
/// replay-equal. `await_playout` completion is surfaced only as a daemon-side
/// tracing event (D18), never on the receipt or in the hash.
pub(crate) fn build_inject_audio_receipt(action_id: u64, session_id: &str) -> Receipt {
    let mut r = build_recording_started_receipt(action_id, session_id);
    r.outcome_hash = Some(loom_core::content_store::sha256_hex(
        b"loom:audio:inject-ok",
    ));
    r
}

/// voice-call-io (task 09): success receipt for `web.say`. TTS output is injected
/// through the ordinary `inject_audio` path, so this carries the SAME constant
/// enqueue-success `outcome_hash` shape as `build_inject_audio_receipt` (a distinct
/// per-verb marker keeps the manifest hash chain replay-equal; playout completion is
/// a daemon tracing event, never a receipt field — D18). Replay-exclusion is inherited
/// from the inject path — `web.say` adds no new replay wiring.
pub(crate) fn build_say_receipt(action_id: u64, session_id: &str) -> Receipt {
    let mut r = build_recording_started_receipt(action_id, session_id);
    r.outcome_hash = Some(loom_core::content_store::sha256_hex(b"loom:audio:say-ok"));
    r
}

/// voice-call-io (task 06): success receipt for `web.start_audio_capture` (capture
/// began; the WAV hash arrives on the stop receipt). Carries a CONSTANT per-verb
/// `outcome_hash` dispatch marker (PRD D6), mirroring `build_inject_audio_receipt`.
pub(crate) fn build_audio_capture_started_receipt(action_id: u64, session_id: &str) -> Receipt {
    let mut r = build_recording_started_receipt(action_id, session_id);
    r.outcome_hash = Some(loom_core::content_store::sha256_hex(
        b"loom:audio:capture-start-ok",
    ));
    r
}

/// voice-call-io (task 06): `web.stop_audio_capture` receipt. On success carries the
/// observational `audio_after_hash` (the captured `.wav` CAS hash), a CONSTANT
/// per-verb `outcome_hash`, and `audio_stop_reason` (so a `byte_cap`/`duration_cap`
/// truncation is caller-observable, not just a server log). The WAV bytes live in
/// CAS OUTSIDE the manifest hash chain (host-intercept, like screencast), so capture
/// never affects replay-equality. A capture that produced no audio (no inbound track
/// / mux error / host over-ceiling reject) → an error receipt carrying the
/// stop_reason + error; the session itself is never aborted.
pub(crate) fn build_stop_audio_capture_receipt(
    action_id: u64,
    session_id: &str,
    result: loom_host::wasm_host::wasm_host::AudioCaptureResult,
) -> Receipt {
    match result.audio_after_hash {
        Some(hash) => {
            let mut r = build_recording_started_receipt(action_id, session_id);
            r.outcome_hash = Some(loom_core::content_store::sha256_hex(
                b"loom:audio:capture-stop-ok",
            ));
            r.audio_after_hash = Some(hash);
            r.audio_stop_reason = Some(result.stop_reason);
            r
        }
        None => {
            // Preserve the typed stop_reason on the error receipt too (C4/#18):
            // a caller seeing `no_inbound_track` / `no_samples` / `session_closed`
            // gets the reason, not only a free-text message.
            let stop_reason = result.stop_reason.clone();
            let mut r = recording_error_receipt(
                action_id,
                session_id,
                "audio_capture_failed",
                result
                    .error
                    .unwrap_or_else(|| format!("capture produced no audio ({stop_reason})")),
            );
            r.audio_stop_reason = Some(stop_reason);
            r
        }
    }
}

/// Map a failed `web.inject_audio` (the `LoomError` message threaded up from the
/// shim's typed `detail`) to a typed receipt `error.kind`. The shim emits the bare
/// kind in `ShimResponse::Error.detail`; it arrives here embedded in the host error
/// string (`"shim chromium:<sid>: <kind>"`), so match on substrings. Unknown →
/// `inject_failed` (never silently succeeds).
pub(crate) fn classify_inject_error(message: &str) -> &'static str {
    for kind in [
        "no_microphone_request",
        "audio_decode_failed",
        "audio_not_enabled",
        "inject_timeout",
        "audio_bridge_unavailable",
        "payload_too_large",
        "invalid_argument",
        "blob_not_found",
        "determinism_enabled",
    ] {
        if message.contains(kind) {
            return kind;
        }
    }
    "inject_failed"
}

pub(crate) fn cookie_validation_error_receipt(
    action_id: u64,
    session_id: &str,
    code: &str,
    message: String,
) -> Receipt {
    use loom_rpc::host_service_adapter::host_service_adapter::{ReceiptError, ReceiptStatus};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Receipt {
        action_id,
        session_id: session_id.to_string(),
        status: ReceiptStatus::Error,
        timing_ticks: 0,
        side_effects: vec![],
        error: Some(ReceiptError {
            kind: "cookie_validation_error".to_string(),
            detail: Some(serde_json::json!({
                "code": code,
                "message": message,
            })),
        }),
        action_hash: None,
        outcome_hash: None,
        emitted_at_ms: Some(now),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        dom_after_hash: None,
        screenshot_after_hash: None,
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: None,
        network_count: None,
        console_lines: vec![],
        network_summary: None,
        network_entries: vec![],
        network_entries_blob_ref: None,
        network_entries_truncated: None,
        settle_until: None,
        settle_outcome: None,
        return_value_json: None,
        return_value_blob_ref: None,
        set_cookies_result: None,
        get_cookies_result: None,
        clear_cookies_result: None,
        delete_cookies_result: None,
        scroll_result: None,
    }
}

/// v0.9.7 follow-up: map a typed `CookieValidationError` variant to the
/// snake_case error-code string used on the wire error receipt.
pub(crate) fn cookie_validation_code(
    e: &loom_shared::cookie_types::CookieValidationError,
) -> &'static str {
    use loom_shared::cookie_types::CookieValidationError as E;
    match e {
        E::NameEmpty => "name_empty",
        E::NameInvalid { .. } => "name_invalid",
        E::ValueTooLarge { .. } => "value_too_large",
        E::InvalidSameSite(_) => "invalid_same_site",
        E::InvalidExpires(_) => "invalid_expires",
        E::TooManyCookies(_) => "too_many_cookies",
    }
}

/// Build a CBOR-encoded `CdpMessage` envelope for the given Web.* action.
/// Returns None for actions that don't have a CDP method mapping yet
/// (caller falls back to the legacy JCS-Action encoding).
///
/// This shape MUST match `loom_shared::shim_protocol::CdpMessage` so
/// `ShimManager::send` can decode the bytes via `ciborium_from_slice`.
/// v0.9.6 helper: convert a `serde_json::Value` (typically a cookie object
/// in `web.set_cookies`'s `source.cookies[]`) into a `ciborium::value::Value`
/// for direct embedding in the CDP CBOR envelope. Returns None for shapes
/// the CDP wire can't represent (e.g. arbitrary nested arrays in places
/// chromium expects scalars) — caller drops the entry rather than the
/// whole batch.
pub(crate) fn serde_json_value_to_cbor(v: serde_json::Value) -> Option<ciborium::value::Value> {
    use ciborium::value::Value;
    use serde_json::Value as J;
    match v {
        J::Null => Some(Value::Null),
        J::Bool(b) => Some(Value::Bool(b)),
        J::String(s) => Some(Value::Text(s)),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Integer(i.into()))
            } else if let Some(u) = n.as_u64() {
                Some(Value::Integer((u as i128).try_into().ok()?))
            } else {
                n.as_f64().map(Value::Float)
            }
        }
        J::Array(arr) => Some(Value::Array(
            arr.into_iter()
                .filter_map(serde_json_value_to_cbor)
                .collect(),
        )),
        J::Object(obj) => Some(Value::Map(
            obj.into_iter()
                .filter_map(|(k, v)| Some((Value::Text(k), serde_json_value_to_cbor(v)?)))
                .collect(),
        )),
    }
}

/// Build the JS expression for `web.scroll`. Targets the viewport
/// (`document.scrollingElement`) when the selector is absent, empty,
/// non-matching, or refers to `body`/`html`/the document element; otherwise
/// scrolls the resolved element. Returns `{x: window.scrollX, y: window.scrollY}`
/// so the post-scroll viewport position can be surfaced on the receipt via the
/// evaluate tier (the guest `scroll_verb` runs this through `evaluate_execute`).
///
/// `selector` is embedded via `serde_json::to_string` — a JSON string literal
/// (e.g. `"body"`) or `null` when absent — so a selector containing `"` or `\`
/// (the only user-controlled input) cannot break out of the JS string. Wrapped
/// in an IIFE so the multi-statement body is a single expression whose value
/// `Runtime.evaluate` returns.
pub(crate) fn build_scroll_expression(
    selector: &Option<String>,
    delta_x: i64,
    delta_y: i64,
) -> String {
    // `null` (no selector) or a JSON string literal like `"body"`.
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "null".to_string());
    format!(
        "(()=>{{const el={sel}?document.querySelector({sel}):null;\
         const box=(!el||el===document.body||el===document.documentElement)\
         ?(document.scrollingElement||document.documentElement):el;\
         box.scrollBy({delta_x},{delta_y});\
         return{{x:window.scrollX,y:window.scrollY}};}})()"
    )
}

/// cdp-trusted-input: how a `web.type` invocation dispatches, by `mode`. The
/// SINGLE source of truth shared by the host-side intercept (`wasm_bridge`) and
/// the value-mode JS builder (`build_chromium_args`), so the two routers never
/// drift (see decisions.md D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebTypeDispatch {
    /// Default (`mode` absent or `"fill"`): host-side CDP `Input.insertText`
    /// (Playwright `fill()` — genuine `isTrusted:true` edit that drives a
    /// framework's `onChange`/react-hook-form state).
    Fill,
    /// `mode:"keystrokes"`: host-side per-char `Input.dispatchKeyEvent`.
    Keystrokes,
    /// `mode:"value"` (or any unrecognized string → back-compat): WASM-guest
    /// `Runtime.evaluate` prototype-setter + synthetic `input`/`change` events.
    ValueGuest,
}

/// Classify a `web.type` `mode` into its dispatch path. `None`/`"fill"` → `Fill`
/// (the default after the flip); `"keystrokes"` → `Keystrokes`; everything else
/// — `"value"` AND any unknown string — → `ValueGuest`, preserving the pre-flip
/// "unknown → value" behavior (decisions.md D5/D8; council: don't error on an
/// unknown mode).
pub(crate) fn classify_web_type_mode(mode: Option<&str>) -> WebTypeDispatch {
    match mode {
        None | Some("fill") => WebTypeDispatch::Fill,
        Some("keystrokes") => WebTypeDispatch::Keystrokes,
        _ => WebTypeDispatch::ValueGuest,
    }
}

/// cdp-trusted-input: receipt for a trusted-input verb (`web.type`
/// fill/keystrokes, `web.press_key`, trusted `web.click`). `Ok` → Success with
/// a CONSTANT `outcome_hash` dispatch-success marker (NOT page-state-bearing, so
/// the manifest hash chain stays replay-equal, exactly like the existing
/// interaction verbs). Application outcomes map to typed error `kind`s. The
/// real `Input.*` side effects happen at record time only; replay is structural.
pub(crate) fn build_input_dispatch_receipt(
    action_id: u64,
    session_id: &str,
    action: &Action,
    outcome: loom_host::shim_manager::InputDispatchOutcome,
) -> Receipt {
    use loom_host::shim_manager::InputDispatchOutcome as O;
    let mut r = match outcome {
        // `DispatchedAckPending` (the committing input was sent but its ack was
        // lost to a cross-origin renderer swap) is a PERFORMED dispatch — it maps
        // to the SAME success template + SAME constant `outcome_hash` marker as
        // `Ok`, so the hashed receipt bytes are IDENTICAL whether the ack arrived
        // or was lost. Record-time ack timing therefore never perturbs the
        // manifest hash chain → replay stays bit-equal (NFR-DET-01). The degraded
        // readiness rides observationally on `settle_outcome` (off the chain).
        O::Ok | O::DispatchedAckPending => {
            // Reuse the all-None success template, then stamp the constant marker.
            let mut r = build_recording_started_receipt(action_id, session_id);
            r.outcome_hash = Some(loom_core::content_store::sha256_hex(
                b"loom:trusted-input:dispatch-ok",
            ));
            r
        }
        O::SelectorNotFound => recording_error_receipt(
            action_id,
            session_id,
            "selector_not_found",
            "selector matched no element".to_string(),
        ),
        O::NotHittable => recording_error_receipt(
            action_id,
            session_id,
            "not_hittable",
            "element has no box model (display:none / detached / zero-size)".to_string(),
        ),
        O::UnknownKey => recording_error_receipt(
            action_id,
            session_id,
            "unknown_key",
            "unknown key name or modifier".to_string(),
        ),
        // Fixed wording only: neither the typed text (F7) nor anything the page
        // produced reaches the message.
        O::MalformedValue(input_type) => recording_error_receipt(
            action_id,
            session_id,
            "malformed_value",
            format!(
                "the {} input rejected the value; it keeps {}",
                input_type.as_str(),
                input_type.expected_format()
            ),
        ),
        O::NotEditable => recording_error_receipt(
            action_id,
            session_id,
            "not_editable",
            "element is disabled or readonly".to_string(),
        ),
    };
    // Stamp the action_hash so the host-side input verbs carry the same receipt
    // contract as the guest-dispatched interaction verbs (every interaction
    // receipt has `action_hash`). Session-independent → replay-equal.
    r.action_hash = Some(input_action_hash(action));
    r
}

/// interactive-settle-bounded: fold the BOUNDED post-action settle verdict onto
/// an already-built input-dispatch receipt (`web.click` / `web.type`
/// fill/keystrokes). Stamps ONLY the observational readiness fields
/// (`settle_until` + `settle_outcome`) and leaves `outcome_hash` / `action_hash`
/// untouched, so the verdict rides observationally and the manifest hash chain
/// stays replay-equal (NFR-DET-01) — exactly as navigate/wait_for exclude their
/// settle diagnostics from the hash. `settle_ms` / `network_count_at_settle` are
/// wall/virtual-time diagnostics with no receipt field; the caller logs them.
pub(crate) fn stamp_settle_outcome(
    receipt: &mut Receipt,
    outcome: &loom_shared::navigate_outcome::WaitOutcome,
) {
    receipt.settle_until = Some(outcome.settle_until.clone());
    receipt.settle_outcome = Some(outcome.settle_outcome.clone());
}

/// Build the receipt for the host-intercepted `web.wait` verb. Mirrors
/// [`build_input_dispatch_receipt`]: a `Resolved` wait reuses the all-None success
/// template + a constant `outcome_hash` marker; a `PredicateFalse` (deadline
/// elapsed) maps to the typed `kind: "wait_predicate_false"` error receipt — the
/// SAME wire kind the old guest path surfaced on a missed selector. The
/// `action_hash` is session-independent (`web.wait\0{selector}`) so replay stays
/// hash-equal regardless of poll timing (only the verdict is recorded, never the
/// poll count).
pub(crate) fn build_wait_receipt(
    action_id: u64,
    session_id: &str,
    action: &Action,
    outcome: loom_host::shim_manager::WaitResolveOutcome,
) -> Receipt {
    use loom_host::shim_manager::WaitResolveOutcome as W;
    let mut r = match outcome {
        W::Resolved => {
            let mut r = build_recording_started_receipt(action_id, session_id);
            r.outcome_hash = Some(loom_core::content_store::sha256_hex(b"loom:wait:resolved"));
            r
        }
        W::PredicateFalse => recording_error_receipt(
            action_id,
            session_id,
            "wait_predicate_false",
            "selector did not appear before timeout".to_string(),
        ),
    };
    r.action_hash = Some(input_action_hash(action));
    r
}

/// Deterministic, **session-independent** `action_hash` for the host-side verbs
/// (web.click / web.type keystrokes / web.press_key / web.wait). Hashes the verb +
/// its input params (NOT `session_id`) so the same script replays to the same
/// hash across sessions, matching how the guest derives `action_hash` from the
/// canonical CDP payload (which also excludes the session). `timeout_ms` is a
/// wall-clock budget, not part of a wait's identity, so it is excluded (two waits
/// for the same selector with different timeouts are the same logical action).
fn input_action_hash(action: &Action) -> String {
    let canonical = match action {
        Action::WebClick { selector, .. } => format!("web.click\u{0}{selector}"),
        Action::WebWait { selector, .. } => format!("web.wait\u{0}{selector}"),
        Action::WebType {
            selector,
            text,
            mode,
            ..
        } => format!(
            "web.type\u{0}{}\u{0}{selector}\u{0}{text}",
            mode.as_deref().unwrap_or("value")
        ),
        Action::WebPressKey {
            key,
            selector,
            modifiers,
            ..
        } => format!(
            "web.press_key\u{0}{key}\u{0}{}\u{0}{}",
            selector.as_deref().unwrap_or(""),
            modifiers.as_ref().map(|m| m.join(",")).unwrap_or_default()
        ),
        // Unreachable for the input verbs; a stable fallback keeps it total.
        other => format!("{other:?}"),
    };
    loom_core::content_store::sha256_hex(canonical.as_bytes())
}

pub(crate) fn build_chromium_args(action: &Action) -> Option<Vec<u8>> {
    use ciborium::value::Value;

    // Build the `Runtime.evaluate` envelope for selector-driven verbs.
    // `expression` is built from CLI-provided strings (selector / text /
    // value), so we MUST embed them as JSON-encoded string literals
    // (`serde_json::to_string`) — naive `format!("{s}")` would let
    // a `"` in the selector break out of the JS string.
    let runtime_evaluate = |expression: String| -> Value {
        Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Runtime.evaluate".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![
                    (Value::Text("expression".into()), Value::Text(expression)),
                    (Value::Text("returnByValue".into()), Value::Bool(true)),
                    (Value::Text("awaitPromise".into()), Value::Bool(false)),
                ]),
            ),
        ])
    };

    let msg = match action {
        Action::WebNavigate { url, .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Page.navigate".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![
                    (Value::Text("url".into()), Value::Text(url.clone())),
                    (
                        Value::Text("transitionType".into()),
                        Value::Text("typed".into()),
                    ),
                ]),
            ),
        ]),

        // cdp-trusted-input: web.click is ALWAYS trusted now — intercepted
        // host-side (CDP Input.dispatchMouseEvent at the element hit point),
        // like recording. No guest Runtime.evaluate args. Handled in wasm_bridge
        // before build_chromium_args is reached; this arm satisfies the match.
        Action::WebClick { .. } => return None,

        Action::WebEvaluate { expression, .. } => runtime_evaluate(expression.clone()),

        // web.set_input_files uses the typed `set_input_files_execute` host
        // function (like navigate/evaluate), NOT a single CDP envelope —
        // `args_canonical_bytes` special-cases it before this fn is called.
        // This arm exists only to satisfy the exhaustive match.
        Action::WebSetInputFiles { .. } => return None,
        // Intercepted before build_chromium_args (host/shim read, no CDP).
        Action::WebNetworkLog { .. } => return None,
        // video-capture: recording is a streaming flow (start → frames → stop),
        // not a single CDP command, so it has no direct-CDP args envelope here —
        // handled by the surface `start-recording`/`stop-recording` verbs.
        Action::WebStartRecording { .. } | Action::WebStopRecording { .. } => return None,
        // cdp-trusted-input: web.press_key is a host-side CDP Input.* verb (no
        // guest); intercepted in wasm_bridge before build_chromium_args.
        Action::WebPressKey { .. } => return None,
        // voice-call-io: audio verbs are host-side intercepts (no direct-CDP
        // envelope), like network_log/recording — intercepted in wasm_bridge before
        // build_chromium_args, so they have no CDP-replay args (None). (`WebSay` is
        // task 09; it too resolves to InjectAudio host-side.)
        Action::WebInjectAudio { .. }
        | Action::WebStartAudioCapture { .. }
        | Action::WebStopAudioCapture { .. }
        | Action::WebSay { .. } => return None,

        Action::WebType {
            selector,
            text,
            mode,
            ..
        } => {
            // cdp-trusted-input: the default `fill` mode and `keystrokes` are
            // intercepted host-side in wasm_bridge (CDP Input.insertText /
            // dispatchKeyEvent); only the legacy `value` mode (and any unknown
            // string → value, back-compat) builds the Runtime.evaluate args here.
            // Single source of truth: classify_web_type_mode (decisions.md D8).
            if classify_web_type_mode(mode.as_deref()) != WebTypeDispatch::ValueGuest {
                return None;
            }
            // Direct `el.value = text` bypasses React/Vue/Angular value
            // trackers — the DOM `.value` is set but framework state still
            // thinks the field is empty, so a follow-up form submit fails
            // with "this field is required". Use the native prototype
            // setter so the framework's tracker fires its change observer.
            // (Same approach Playwright/testing-library use for the same
            // reason.)
            let sel = serde_json::to_string(selector).ok()?;
            let val = serde_json::to_string(text).ok()?;
            runtime_evaluate(format!(
                "(function(){{\
                  const el=document.querySelector({sel});\
                  el.focus();\
                  const proto=el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;\
                  const setter=Object.getOwnPropertyDescriptor(proto,'value').set;\
                  setter.call(el,{val});\
                  el.dispatchEvent(new Event('input',{{bubbles:true}}));\
                  el.dispatchEvent(new Event('change',{{bubbles:true}}));\
                }})()"
            ))
        }

        Action::WebSelect {
            selector, value, ..
        } => {
            // Same React/Vue/Angular tracker problem as web.type — the
            // native HTMLSelectElement setter is what frameworks observe.
            let sel = serde_json::to_string(selector).ok()?;
            let val = serde_json::to_string(value).ok()?;
            runtime_evaluate(format!(
                "(function(){{\
                  const el=document.querySelector({sel});\
                  const setter=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set;\
                  setter.call(el,{val});\
                  el.dispatchEvent(new Event('input',{{bubbles:true}}));\
                  el.dispatchEvent(new Event('change',{{bubbles:true}}));\
                }})()"
            ))
        }

        Action::WebHover { selector, .. } => {
            let sel = serde_json::to_string(selector).ok()?;
            runtime_evaluate(format!(
                "document.querySelector({sel}).dispatchEvent(\
                 new MouseEvent('mouseover',{{bubbles:true,cancelable:true}}))"
            ))
        }

        Action::WebScroll {
            selector,
            delta_x,
            delta_y,
            ..
        } => {
            // Dead-for-dispatch: scroll uses the raw-expression `args_canonical_bytes`
            // path (like WebEvaluate), not this CBOR envelope. Kept for the
            // `build_chromium_args_*` tests + parity with the WebEvaluate arm.
            // Single source of the scroll JS = `build_scroll_expression`.
            runtime_evaluate(build_scroll_expression(
                selector,
                delta_x.unwrap_or(0),
                delta_y.unwrap_or(0),
            ))
        }

        // web.wait is now intercepted host-side (like web.click): the daemon polls
        // the locator via `host.wait` → `send_wait` (reusing the `resolve_locator_node`
        // grammar resolver), so there is no guest Runtime.evaluate envelope. Handled
        // in wasm_bridge before build_chromium_args is reached; this arm satisfies
        // the match. (The old raw `querySelector(sel)` probe threw on text=/role=.)
        Action::WebWait { .. } => return None,

        // settle-capture: web.wait_for uses the typed `wait_for_execute` host
        // function (like navigate/evaluate), NOT a single CDP envelope —
        // `args_canonical_bytes` special-cases it before this fn is called.
        // This arm exists only to satisfy the exhaustive match.
        Action::WebWaitFor { .. } => return None,

        Action::WebScreenshot { .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Page.captureScreenshot".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![(
                    Value::Text("format".into()),
                    Value::Text("png".into()),
                )]),
            ),
        ]),

        Action::WebSnapshot { .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("DOM.getDocument".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![
                    (
                        Value::Text("depth".into()),
                        Value::Integer((-1i128).try_into().ok()?),
                    ),
                    // pierce:true inlines shadow-DOM + iframe contentDocument subtrees,
                    // matching web.navigate (shim STEP 5) so the two DOM captures hash a
                    // comparable node set. Normalized via dom_normalize (frameId stripped
                    // recursively) at the shim cdp_send chokepoint.
                    (Value::Text("pierce".into()), Value::Bool(true)),
                ]),
            ),
        ]),

        // v0.9.6 web-cookie-injection: build CDP envelopes daemon-side
        // (the WASM verbs in loom-surface-web forward whatever
        // action.payload they receive via host::shim_call, so we need
        // the payload to be a valid CDP CBOR envelope by the time it
        // reaches the chromium shim).
        //
        // Per-cookie validation (validate_cookie_params) is intentionally
        // NOT performed on this raw-JSON `source` path — it would require
        // converting the untyped `source` into typed `loom_shared::cookie_types`
        // structs first. The inline path validates via the typed
        // `validate_cookie_params` above; on this path the chromium shim's
        // Network.setCookies response surfaces individual cookie rejections.
        //
        // Grant resolution is now performed upstream in
        // `dispatch_action_blocking` (v0.9.7 follow-up A) — by the
        // time we get here the source should always be `inline`.
        // The non-inline branch below is defensive: if some other
        // caller (e.g. tests) hands us a `grant` source we emit an
        // empty no-op envelope rather than trapping.
        Action::WebSetCookies { source, .. } => {
            // source = {"source":"inline","cookies":[...]} or
            // {"source":"grant","grant_id":"..."}
            let kind = source.get("source").and_then(|v| v.as_str())?;
            if kind != "inline" {
                tracing::warn!(
                    "build_chromium_args saw set_cookies with non-inline source after dispatcher should have resolved it; emitting empty Network.setCookies",
                );
                return Some({
                    let v = Value::Map(vec![
                        (
                            Value::Text("method".into()),
                            Value::Text("Network.setCookies".into()),
                        ),
                        (
                            Value::Text("params".into()),
                            Value::Map(vec![(Value::Text("cookies".into()), Value::Array(vec![]))]),
                        ),
                    ]);
                    let mut bytes = Vec::new();
                    ciborium::ser::into_writer(&v, &mut bytes).ok()?;
                    bytes
                });
            }
            let cookies = source
                .get("cookies")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| serde_json_value_to_cbor(c.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Value::Map(vec![
                (
                    Value::Text("method".into()),
                    Value::Text("Network.setCookies".into()),
                ),
                (
                    Value::Text("params".into()),
                    Value::Map(vec![(Value::Text("cookies".into()), Value::Array(cookies))]),
                ),
            ])
        }
        Action::WebGetCookies { urls, .. } => {
            let mut params: Vec<(Value, Value)> = vec![];
            if let Some(u) = urls {
                params.push((
                    Value::Text("urls".into()),
                    Value::Array(u.iter().map(|s| Value::Text(s.clone())).collect()),
                ));
            }
            Value::Map(vec![
                (
                    Value::Text("method".into()),
                    Value::Text("Network.getCookies".into()),
                ),
                (Value::Text("params".into()), Value::Map(params)),
            ])
        }
        Action::WebClearCookies { .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Network.clearBrowserCookies".into()),
            ),
            (Value::Text("params".into()), Value::Map(vec![])),
        ]),
        Action::WebDeleteCookies {
            name,
            url,
            domain,
            path,
            ..
        } => {
            let mut params: Vec<(Value, Value)> =
                vec![(Value::Text("name".into()), Value::Text(name.clone()))];
            if let Some(u) = url {
                params.push((Value::Text("url".into()), Value::Text(u.clone())));
            }
            if let Some(d) = domain {
                params.push((Value::Text("domain".into()), Value::Text(d.clone())));
            }
            if let Some(p) = path {
                params.push((Value::Text("path".into()), Value::Text(p.clone())));
            }
            Value::Map(vec![
                (
                    Value::Text("method".into()),
                    Value::Text("Network.deleteCookies".into()),
                ),
                (Value::Text("params".into()), Value::Map(params)),
            ])
        }
    };

    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&msg, &mut bytes).ok()?;
    Some(bytes)
}

/// Promote a `web.scroll` receipt's evaluate-tier return value into the
/// purpose-named `scroll_result` field. The value is the canonical-JSON `{x,y}`
/// the guest's `scroll_verb` produced via `evaluate_execute`. On success the
/// value moves to `scroll_result` and `return_value_json` is cleared, so a
/// scroll receipt has a single source of truth.
///
/// If the value somehow does not parse as JSON (practically impossible — the
/// host always emits canonical JSON, and `{x,y}` never exceeds the inline-offload
/// threshold), `return_value_json` is left intact rather than silently dropped.
pub(crate) fn promote_scroll_result(receipt: &mut Receipt) {
    if let Some(parsed) = receipt
        .return_value_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
    {
        receipt.scroll_result = Some(parsed);
        receipt.return_value_json = None;
    }
}

/// Construct the wire `Receipt` for a successful action outcome.
///
/// Decodes the three navigate JSON blobs (`navigate_*_json`) into
/// typed wire fields; degrades to empty / None with `tracing::warn` on
/// decode failure — observability fields shouldn't fail the navigate.
/// Applies `apply_capture_profile_to_wire` last so `--capture-policy
/// minimal` strips tier-2 fields per.
pub(crate) fn build_navigate_wire_receipt(
    builder: &loom_host::receipt_marshaller::ReceiptBuilder,
    session_id: &str,
    capture_policy_str: Option<&str>,
) -> Receipt {
    use loom_host::receipt_marshaller::ReceiptStatus as HostStatus;
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;

    let status = match builder.status {
        HostStatus::Ok => ReceiptStatus::Success,
        _ => ReceiptStatus::Error,
    };
    let action_hash = (!builder.action_hash.is_empty()).then(|| builder.action_hash.clone());
    let outcome_hash = (!builder.outcome_hash.is_empty()).then(|| builder.outcome_hash.clone());
    let emitted_at_ms = (builder.emitted_at_ms != 0).then_some(builder.emitted_at_ms);

    // decode shim-captured network events from the
    // WIT side-effects-json escape hatch onto the wire receipt's typed
    // `side_effects[]` array.
    let side_effects: Vec<serde_json::Value> = builder
        .navigate_side_effects_json
        .as_deref()
        .map(|bytes| {
            match serde_json::from_slice::<Vec<loom_shared::navigate_outcome::LoomNetworkEvent>>(
                bytes,
            ) {
                Ok(events) => events
                    .into_iter()
                    .filter_map(|e| serde_json::to_value(&e).ok())
                    .collect(),
                Err(e) => {
                    tracing::warn!(
                        action_id = builder.action_id,
                        error = %e,
                        "navigate receipt: side_effects decode failed; emitting empty"
                    );
                    Vec::new()
                }
            }
        })
        .unwrap_or_default();

    // console_lines verbatim.
    let console_lines: Vec<loom_shared::navigate_outcome::ShimConsoleLine> = builder
        .navigate_console_lines_json
        .as_deref()
        .map(|bytes| match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    action_id = builder.action_id,
                    error = %e,
                    "navigate receipt: console_lines decode failed; emitting empty"
                );
                Vec::new()
            }
        })
        .unwrap_or_default();

    // typed NetworkSummary aggregate.
    let network_summary: Option<loom_core::receipt_builder::receipt_builder::NetworkSummary> =
        builder
            .navigate_network_summary_json
            .as_deref()
            .and_then(|bytes| match serde_json::from_slice(bytes) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        action_id = builder.action_id,
                        error = %e,
                        "navigate receipt: network_summary decode failed; emitting None"
                    );
                    None
                }
            });

    // surface the evaluate return value on the
    // wire. The host's `evaluate_execute` populates
    // `evaluate_return_value_json` (inline-sized) OR
    // `evaluate_return_value_blob_ref` (offloaded to content store);
    // for non-evaluate actions both are `None` and the fields are
    // skipped on serialisation. `blob_ref.sha256` is the wire form so
    // CLI consumers can fetch via `loom blob get`.
    let return_value_json = builder.evaluate_return_value_json.clone();
    let return_value_blob_ref = builder
        .evaluate_return_value_blob_ref
        .as_ref()
        .map(|cref| cref.sha256.clone());

    // Observational network-entries side-channel (NOT hash-chained). Decode the
    // inline JSON bytes into `Vec<Value>`; when offloaded, the bytes are absent
    // and `network_entries_blob_ref` carries the sha256 instead.
    let network_entries: Vec<serde_json::Value> = builder
        .navigate_network_entries_json
        .as_deref()
        .map(|bytes| {
            match serde_json::from_slice::<Vec<loom_shared::navigate_outcome::LoomNetworkEntry>>(
                bytes,
            ) {
                Ok(entries) => entries
                    .into_iter()
                    .filter_map(|e| serde_json::to_value(&e).ok())
                    .collect(),
                Err(e) => {
                    tracing::warn!(
                        action_id = builder.action_id,
                        error = %e,
                        "navigate receipt: network_entries decode failed; emitting empty"
                    );
                    Vec::new()
                }
            }
        })
        .unwrap_or_default();
    let network_entries_blob_ref = builder
        .navigate_network_entries_blob_ref
        .as_ref()
        .map(|cref| cref.sha256.clone());
    let network_entries_truncated = builder.navigate_network_entries_truncated;

    let mut receipt = Receipt {
        action_id: builder.action_id,
        session_id: session_id.to_string(),
        status,
        timing_ticks: builder.finished_at_ms.saturating_sub(builder.started_at_ms),
        side_effects,
        error: builder
            .error_code
            .as_ref()
            .map(|c| build_wire_receipt_error(c, builder.error_details.as_deref())),
        action_hash,
        outcome_hash,
        emitted_at_ms,
        url: builder.navigate_url.clone(),
        final_url: builder.navigate_final_url.clone(),
        title: builder.navigate_title.clone(),
        status_code: builder.navigate_status_code,
        dom_snapshot_hash: builder.navigate_dom_snapshot_hash.clone(),
        // Interaction fingerprint (capture-policy=fingerprint). None for navigate
        // and for non-fingerprint sessions (the host accept-gate already cleared
        // it on the builder otherwise). Surfaces the manifest field on the wire.
        dom_after_hash: builder.interaction_dom_after_hash.clone(),
        screenshot_after_hash: builder.navigate_screenshot_after_hash.clone(),
        screencast_after_hash: None,
        audio_after_hash: None,
        audio_stop_reason: None,
        console_count: builder.navigate_console_count,
        network_count: builder.navigate_network_count,
        console_lines,
        network_summary,
        network_entries,
        network_entries_blob_ref,
        network_entries_truncated,
        // settle-capture readiness fields (surfaced from the builder).
        settle_until: builder.navigate_settle_until.clone(),
        settle_outcome: builder.navigate_settle_outcome.clone(),
        return_value_json,
        return_value_blob_ref,
        // v0.9.6 cookie-result wire fields. `get_cookies_result` is populated
        // from `builder.get_cookies_result` (the host decodes the
        // Network.getCookies response in `shim_call`; SessionExecutor moves it
        // onto the builder). Values are RAW here per D7 (operator-facing
        // receipts include values; the replay hash chain redacts them). The
        // remaining three verbs (set/clear/delete) still forward opaquely and
        // stay `None`.
        set_cookies_result: None,
        get_cookies_result: builder
            .get_cookies_result
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
        clear_cookies_result: None,
        delete_cookies_result: None,
        scroll_result: None,
    };

    // apply per-session capture-policy at the wire
    // boundary. Unknown / unset values → CaptureProfile::Default (no-op).
    let profile = capture_policy_str
        .and_then(loom_core::receipt_builder::receipt_builder::capture_profile_from_str)
        .unwrap_or(loom_core::receipt_builder::receipt_builder::CaptureProfile::Default);
    loom_rpc::host_service_adapter::wire_capture::apply_capture_profile_to_wire(
        &mut receipt,
        profile,
    );
    tracing::debug!(
        action_id = receipt.action_id,
        ?profile,
        "navigate receipt: capture-policy applied"
    );
    receipt
}

/// Build the wire `ReceiptError` from the host-side `ReceiptBuilder`'s
/// `error_code` + `error_details`. Two shapes feed in:
///
/// 1. **Typed shim failure** — `error_code = "shim-failure"`,
///    `error_details = JSON {"kind": "...", "url": "...", ...}`.
///    Hoists the `kind` field to the wire `ReceiptError.kind` and puts
///    the remaining fields into `detail`.
///
/// 2. **Untyped shim failure or other host error** — kind defaults to
///    the host's `error_code`; `detail` wraps the raw `error_details`
///    string in `{"message": "..."}` (or is omitted when empty).
pub(crate) fn build_wire_receipt_error(
    error_code: &str,
    error_details: Option<&str>,
) -> loom_rpc::host_service_adapter::host_service_adapter::ReceiptError {
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptError;

    if error_code == "shim-failure" {
        if let Some(detail_str) = error_details {
            if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(detail_str) {
                if let Some(kind) = parsed
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .map(String::from)
                {
                    if let Some(obj) = parsed.as_object_mut() {
                        obj.remove("kind");
                    }
                    let detail = match parsed.as_object() {
                        Some(map) if map.is_empty() => None,
                        _ => Some(parsed),
                    };
                    return ReceiptError { kind, detail };
                }
            }
        }
    }
    let detail = error_details
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::json!({ "message": s }));
    ReceiptError {
        kind: error_code.to_string(),
        detail,
    }
}

pub(crate) fn action_session_id(action: &Action) -> &str {
    match action {
        Action::WebNavigate { session_id, .. }
        | Action::WebClick { session_id, .. }
        | Action::WebEvaluate { session_id, .. }
        | Action::WebType { session_id, .. }
        | Action::WebScreenshot { session_id, .. }
        | Action::WebSelect { session_id, .. }
        | Action::WebHover { session_id, .. }
        | Action::WebScroll { session_id, .. }
        | Action::WebWait { session_id, .. }
        | Action::WebSnapshot { session_id } => session_id,
        Action::WebStartRecording { session_id, .. } | Action::WebStopRecording { session_id } => {
            session_id
        }
        Action::WebWaitFor { session_id, .. } => session_id,
        Action::WebSetInputFiles { session_id, .. } => session_id,
        // v0.9.6 web-cookie-injection.
        Action::WebSetCookies { session_id, .. }
        | Action::WebGetCookies { session_id, .. }
        | Action::WebClearCookies { session_id }
        | Action::WebDeleteCookies { session_id, .. } => session_id,
        Action::WebNetworkLog { session_id } => session_id,
        Action::WebPressKey { session_id, .. } => session_id,
        // voice-call-io: audio verbs (surface only; dispatch wired in later tasks).
        Action::WebInjectAudio { session_id, .. }
        | Action::WebStartAudioCapture { session_id, .. }
        | Action::WebStopAudioCapture { session_id }
        | Action::WebSay { session_id, .. } => session_id,
    }
}

pub(crate) fn action_surface(_action: &Action) -> &str {
    // Must match the file-stem used by `ModuleLibrary::load_all`
    // (loom-host/src/module_library/interfaces.rs:80) which keys
    // surfaces by the .cwasm file stem. `loom postinstall` produces
    // `loom_surface_web.cwasm`, so the lookup is `SurfaceName("loom_surface_web")`.
    "loom_surface_web"
}

pub(crate) fn action_verb(action: &Action) -> &str {
    // Must match the WIT export name in `wit/loom-surface.wit` verbatim.
    // `web.type-text` and the v0.9.6 cookie verbs (`set-cookies`,
    // `get-cookies`, `clear-cookies`, `delete-cookies`) are the
    // kebab-cased verbs.
    match action {
        Action::WebNavigate { .. } => "navigate",
        Action::WebClick { .. } => "click",
        Action::WebEvaluate { .. } => "evaluate",
        Action::WebType { .. } => "type-text",
        Action::WebScreenshot { .. } => "screenshot",
        Action::WebSelect { .. } => "select",
        Action::WebHover { .. } => "hover",
        Action::WebScroll { .. } => "scroll",
        Action::WebWait { .. } => "wait",
        Action::WebWaitFor { .. } => "wait-for",
        Action::WebSnapshot { .. } => "snapshot",
        Action::WebStartRecording { .. } => "start-recording",
        Action::WebStopRecording { .. } => "stop-recording",
        Action::WebSetInputFiles { .. } => "set-input-files",
        // v0.9.6 web-cookie-injection.
        Action::WebSetCookies { .. } => "set-cookies",
        Action::WebGetCookies { .. } => "get-cookies",
        Action::WebClearCookies { .. } => "clear-cookies",
        Action::WebDeleteCookies { .. } => "delete-cookies",
        Action::WebNetworkLog { .. } => "network-log",
        // cdp-trusted-input: host-side verb (no WIT export); label for telemetry.
        Action::WebPressKey { .. } => "press-key",
        // voice-call-io: host-side audio verbs (no WIT export); labels for telemetry.
        Action::WebInjectAudio { .. } => "inject-audio",
        Action::WebStartAudioCapture { .. } => "start-audio-capture",
        Action::WebStopAudioCapture { .. } => "stop-audio-capture",
        Action::WebSay { .. } => "say",
    }
}

#[cfg(test)]
mod audio_capture_receipt_tests {
    use super::*;
    use loom_host::wasm_host::wasm_host::AudioCaptureResult;
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;

    fn constant(marker: &[u8]) -> String {
        loom_core::content_store::sha256_hex(marker)
    }

    #[test]
    fn stop_success_carries_hash_reason_and_constant_marker() {
        let hash = "ab".repeat(32);
        let result = AudioCaptureResult {
            audio_after_hash: Some(hash.clone()),
            sample_count: 16_000,
            duration_ms: 1_000,
            dropped_frames: 0,
            source_sample_rate: 48_000,
            stop_reason: "explicit".to_string(),
            error: None,
        };
        let r = build_stop_audio_capture_receipt(7, "sess-1", result);
        assert!(matches!(r.status, ReceiptStatus::Success));
        assert_eq!(r.audio_after_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(r.audio_stop_reason.as_deref(), Some("explicit"));
        // Constant per-verb dispatch marker (PRD D6) — NOT the audio bytes hash, so
        // two different captures chain identically at the manifest layer.
        assert_eq!(
            r.outcome_hash,
            Some(constant(b"loom:audio:capture-stop-ok"))
        );
        assert_ne!(r.outcome_hash.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn stop_surfaces_cap_truncation_reason_to_caller() {
        // C4: a byte_cap/duration_cap truncation is caller-observable on the receipt,
        // not just a server log line.
        let result = AudioCaptureResult {
            audio_after_hash: Some("cd".repeat(32)),
            stop_reason: "byte_cap".to_string(),
            ..Default::default()
        };
        let r = build_stop_audio_capture_receipt(1, "s", result);
        assert_eq!(r.audio_stop_reason.as_deref(), Some("byte_cap"));
    }

    #[test]
    fn stop_with_no_audio_is_typed_error_not_a_kill() {
        // M18: a capture that produced no audio → Error-status receipt (session is
        // never aborted); no audio hash.
        let result = AudioCaptureResult {
            audio_after_hash: None,
            stop_reason: "no_inbound_track".to_string(),
            error: Some("no inbound audio track".to_string()),
            ..Default::default()
        };
        let r = build_stop_audio_capture_receipt(1, "s", result);
        assert!(matches!(r.status, ReceiptStatus::Error));
        assert!(r.audio_after_hash.is_none());
        assert!(r.error.is_some());
    }

    #[test]
    fn markers_are_stable_and_distinct() {
        // M8: start/stop each carry a stable, distinct constant marker across calls.
        let s1 = build_audio_capture_started_receipt(1, "a").outcome_hash;
        let s2 = build_audio_capture_started_receipt(2, "b").outcome_hash;
        assert_eq!(s1, s2, "start marker must be constant across captures");
        assert_eq!(s1, Some(constant(b"loom:audio:capture-start-ok")));
        assert_ne!(
            s1,
            Some(constant(b"loom:audio:capture-stop-ok")),
            "start marker must differ from stop marker"
        );
    }
}
