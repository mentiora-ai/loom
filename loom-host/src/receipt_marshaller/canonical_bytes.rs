// Canonical receipt bytes for the navigate and evaluate verbs (the JCS payload
// `ReceiptMarshaller::assemble_canonical_bytes` routes to). Split out of
// `receipt_marshaller.rs`.

use super::receipt_marshaller::*;
use loom_core::error::LoomError;

/// Build canonical-JSON bytes for navigate tier-2 receipts via
/// `loom_core::ReceiptPayload` so field names match the core schema.
pub(super) fn assemble_navigate_canonical_bytes(
    builder: &ReceiptBuilder,
) -> Result<Vec<u8>, LoomError> {
    use loom_core::error_types::{ReceiptCode, ReceiptSurface};
    use loom_core::receipt_builder::receipt_builder::{
        NetworkEvent, ReceiptPayload, ReceiptStatus,
    };
    use loom_shared::navigate_outcome::LoomNetworkEvent;

    // Invariant (host-side): only the navigate dispatch path on
    // session_executor populates navigate_side_effects_json. If a future
    // web-surface verb (click, type-text, scroll, …) starts emitting
    // side-effects, revisit the gate in assemble_canonical_bytes above.
    debug_assert!(
        builder.navigate_url.is_some()
            || builder.navigate_dom_snapshot_hash.is_some()
            || builder.navigate_side_effects_json.is_some()
            || builder.status == crate::receipt_marshaller::ReceiptStatus::Error,
        "assemble_navigate_canonical_bytes invoked but no navigate signal present on builder"
    );

    // Degraded-path tracing: when the navigate path is
    // taken solely because navigate_side_effects_json is populated (i.e.
    // tier-2 fields are still unset; see navigate-receipt-tier2-still-missing),
    // emit a single warn so operators can see why the resulting receipt has
    // url/title/status_code = null while network_events is populated.
    // No default subscriber is installed during `cargo test`, so this does
    // not pollute test output.
    if builder.navigate_url.is_none()
        && builder.navigate_dom_snapshot_hash.is_none()
        && builder.navigate_side_effects_json.is_some()
        && builder.status != crate::receipt_marshaller::ReceiptStatus::Error
    {
        tracing::warn!(
            action_id = %builder.action_id,
            "navigate receipt sealed with side_effects_json but tier-2 fields unset; \
             HAR will populate from network_events but receipt.url/title/status_code will be null \
             (see navigate-receipt-tier2-still-missing)"
        );
    }

    // Decode shim-captured network events into the canonical receipt's
    // `network_events` so HAR/JSON exporters have per-event url/status/
    // size/mime to render. The bytes here are the
    // JSON encoding of `Vec<LoomNetworkEvent>` written by host_impl.rs.
    // Sub-resource events with `error_reason.is_some()` are mapped too,
    // so DevTools waterfalls can render failed requests; their status
    // comes through verbatim from the shim (0 when no HTTP response).
    let network_events: Vec<NetworkEvent> = builder
        .navigate_side_effects_json
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Vec<LoomNetworkEvent>>(bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|e| NetworkEvent {
            method: e.method,
            url: e.url,
            status_code: u32::from(e.status),
            response_body_sha256_hex: e.response_hash,
            // Determinism (NFR-DET-01): response sizes are volatile
            // (CDN/gzip/chunk boundaries / encodedDataLength) and would break
            // cross-run manifest hash-chain equality. This canonical serialization
            // is BOTH hashed AND read back by the HAR exporter, so by default the
            // size is EXCLUDED (fail-safe: a forgotten flag loses observability,
            // never corrupts the chain). It is kept only for an explicitly
            // non-deterministic session (`preserve_response_sizes`). method/
            // status_code/content_type are deterministic and always kept. The real
            // byte count always rides the non-hashed wire summary (total_bytes) +
            // network_entries regardless.
            response_body_size_bytes: if builder.preserve_response_sizes {
                e.response_bytes
            } else {
                0
            },
            response_body_ref: None,
            // timing_ticks is microseconds; shim
            // duration_ms is milliseconds.
            timing_ticks: e.duration_ms.saturating_mul(1000),
            content_type: e.content_type,
        })
        .collect();

    let is_error = builder.status == crate::receipt_marshaller::ReceiptStatus::Error;

    // For typed-error receipts:
    //  - `code` flips to WebNavigationFailed (still in the stable
    //    ReceiptCode enum).
    //  - `details` is the parsed JSON object the surface emitted
    //    (`{"kind":"http_status","status_code":404,"url":"..."}` or
    //    `{"kind":"dns_failure","url":"...","chromium_error":"..."}`).
    //  - `message` is a SHORT human-readable string (≤ 280 chars)
    //    — NOT the raw JSON blob, which would be hard to
    //    read in operator dashboards.
    let (code, details, message) = if is_error {
        let parsed: Option<serde_json::Value> = builder
            .error_details
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let kind = parsed
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("unknown");
        let friendly = match kind {
            "http_status" => {
                let sc = parsed
                    .as_ref()
                    .and_then(|v| v.get("status_code"))
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0);
                format!("navigate failed: HTTP {sc}")
            }
            // Typed message names the failure mode + helps
            // operators triage (DNS vs reachability vs TLS).
            "dns_failure" => "navigate failed: DNS resolution failed".to_string(),
            "connect_refused" => "navigate failed: connection refused".to_string(),
            "tls_error" => "navigate failed: TLS handshake failed".to_string(),
            "network_error" => "navigate failed: network error".to_string(),
            _ => "navigate failed".to_string(),
        };
        (ReceiptCode::WebNavigationFailed, parsed, Some(friendly))
    } else {
        (
            ReceiptCode::WebActionCompleted,
            None,
            builder.error_details.clone(),
        )
    };

    let status = if is_error {
        ReceiptStatus::Error
    } else {
        ReceiptStatus::Ok
    };

    let payload = ReceiptPayload {
        action_id: builder.action_id.to_string(),
        code,
        details,
        dom_after_hash: None,
        dom_after_blob_ref: None,
        dom_before_blob_ref: None,
        message,
        network_events,
        return_value_json: None,
        return_value_blob_ref: None,
        screenshot_after_hash: builder.navigate_screenshot_after_hash.clone(),
        screenshot_after_blob_ref: None,
        screenshot_before_blob_ref: None,
        screencast_after_hash: None,
        screencast_after_blob_ref: None,
        // voice-call-io: manifest audio fields stay None on the marshaller path —
        // captured audio is intercepted daemon-side (wire receipt), never the guest
        // path, so the manifest chain stays deterministic. Present as the D6
        // replay-exclusion safety net.
        audio_after_hash: None,
        audio_after_blob_ref: None,
        llm_cache_hit: None,
        status,
        surface: ReceiptSurface::Web,
        // timing_ticks unit is microseconds.
        // builder.finished_at_ms is session-elapsed ms from
        // DeterminismHarness::clock_now() (NOT wall-clock UNIX-EPOCH).
        timing_ticks: builder.finished_at_ms.saturating_mul(1000),
        console_lines: Vec::new(),
        url: builder.navigate_url.clone(),
        final_url: builder.navigate_final_url.clone(),
        title: builder.navigate_title.clone(),
        status_code: builder.navigate_status_code,
        dom_snapshot_hash: builder.navigate_dom_snapshot_hash.clone(),
        console_count: builder.navigate_console_count,
        network_count: builder.navigate_network_count,
        emitted_at_ms: if builder.emitted_at_ms > 0 {
            Some(builder.emitted_at_ms)
        } else {
            None
        },
        settle_until: builder.navigate_settle_until.clone(),
        settle_outcome: builder.navigate_settle_outcome.clone(),
    };

    payload.canonical_bytes()
}

/// Build canonical-JSON bytes for evaluate-tier receipts.
/// Mirrors `assemble_navigate_canonical_bytes` shape but for the evaluate
/// path. js_throw / cbor_unrepresentable errors carry typed `details` JSON.
pub(super) fn assemble_evaluate_canonical_bytes(
    builder: &ReceiptBuilder,
) -> Result<Vec<u8>, LoomError> {
    use loom_core::error_types::{ReceiptCode, ReceiptSurface};
    use loom_core::receipt_builder::receipt_builder::{ReceiptPayload, ReceiptStatus};

    let is_error = builder.status == crate::receipt_marshaller::ReceiptStatus::Error;

    // For typed-error evaluate receipts:
    //  - `code` flips to WebActionFailed (existing enum variant).
    //  - `details` is the parsed JSON object the host emitted
    //    (`{"kind":"js_throw","exception":"...","line":N,"column":N}` or
    //    `{"kind":"cbor_unrepresentable","reason":"..."}`).
    //  - `message` is short human-readable (≤ 280 chars).
    let (code, details, message) = if is_error {
        let parsed: Option<serde_json::Value> = builder
            .error_details
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let kind = parsed
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("unknown");
        let (variant_code, friendly) = match kind {
            "js_throw" => {
                // Page-side exception → typed `WebEvaluateThrew`. Surface the
                // exception message into the friendly text so operator
                // dashboards show it without parsing `details`.
                let ex = parsed
                    .as_ref()
                    .and_then(|v| v.get("exception"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("");
                let msg = if ex.is_empty() {
                    "evaluate failed: page-side exception".to_string()
                } else {
                    format!("evaluate failed: {ex}")
                };
                (ReceiptCode::WebEvaluateThrew, msg)
            }
            "cbor_unrepresentable" => {
                // Result shape can't round-trip through canonical-JSON →
                // surface as schema violation.
                let reason = parsed
                    .as_ref()
                    .and_then(|v| v.get("reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown");
                (
                    ReceiptCode::SchemaViolation,
                    format!("evaluate failed: result not representable ({reason})"),
                )
            }
            _ => (ReceiptCode::WebEvaluateThrew, "evaluate failed".to_string()),
        };
        (variant_code, parsed, Some(friendly))
    } else {
        (
            ReceiptCode::WebActionCompleted,
            None,
            builder.error_details.clone(),
        )
    };

    let status = if is_error {
        ReceiptStatus::Error
    } else {
        ReceiptStatus::Ok
    };

    let payload = ReceiptPayload {
        action_id: builder.action_id.to_string(),
        code,
        details,
        dom_after_hash: None,
        dom_after_blob_ref: None,
        dom_before_blob_ref: None,
        message,
        network_events: Vec::new(),
        return_value_json: builder.evaluate_return_value_json.clone(),
        return_value_blob_ref: builder.evaluate_return_value_blob_ref.clone(),
        screenshot_after_hash: None,
        screenshot_after_blob_ref: None,
        screenshot_before_blob_ref: None,
        screencast_after_hash: None,
        screencast_after_blob_ref: None,
        // voice-call-io: manifest audio fields stay None on the marshaller path —
        // captured audio is intercepted daemon-side (wire receipt), never the guest
        // path, so the manifest chain stays deterministic. Present as the D6
        // replay-exclusion safety net.
        audio_after_hash: None,
        audio_after_blob_ref: None,
        llm_cache_hit: None,
        status,
        surface: ReceiptSurface::Web,
        timing_ticks: builder.finished_at_ms.saturating_mul(1000),
        console_lines: Vec::new(),
        url: None,
        final_url: None,
        title: None,
        status_code: None,
        dom_snapshot_hash: None,
        console_count: None,
        network_count: None,
        emitted_at_ms: if builder.emitted_at_ms > 0 {
            Some(builder.emitted_at_ms)
        } else {
            None
        },
        // Non-navigate receipts never carry settle fields.
        settle_until: None,
        settle_outcome: None,
    };

    payload.canonical_bytes()
}
