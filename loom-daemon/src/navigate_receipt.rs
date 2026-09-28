//! The navigate-family wire receipt: translate the host `ReceiptBuilder` output
//! into the `loom-rpc` wire `Receipt` (`build_navigate_wire_receipt`) and its
//! error form (`build_wire_receipt_error`). Split out of `wire_receipts.rs`.

use loom_rpc::host_service_adapter::host_service_adapter::Receipt;

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
