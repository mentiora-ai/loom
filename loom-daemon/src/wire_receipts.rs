//! Wire/receipt builders for the daemon's `WasmBridge` dispatch path.
//!
//! These are pure, leaf helper free functions split out of `lib.rs`
//! (large-file refactor): they synthesize `loom-rpc` wire `Receipt`s for
//! daemon-side gates (profile-restricted evaluate, upload/cookie/recording
//! errors) and the host-intercepted input verbs, and read the
//! surface/verb/session-id off a typed `Action`. Siblings: `guest_args`
//! (guest payload builders), `navigate_receipt`, `media_receipts`.

use crate::guest_args::{classify_web_type_mode, WebTypeDispatch};
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
        O::FillFailed(failure) => recording_error_receipt(
            action_id,
            session_id,
            "type_failed",
            failure.message().to_string(),
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
        // Keyed on the dispatch path actually taken (classify_web_type_mode), so a
        // bare web.type and an explicit `mode:"fill"` — the same fill — hash alike.
        Action::WebType {
            selector,
            text,
            mode,
            ..
        } => {
            let path = match classify_web_type_mode(mode.as_deref()) {
                WebTypeDispatch::Fill => "fill",
                WebTypeDispatch::Keystrokes => "keystrokes",
                WebTypeDispatch::ValueGuest => "value",
            };
            format!("web.type\u{0}{path}\u{0}{selector}\u{0}{text}")
        }
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
