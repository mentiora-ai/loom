// Pure helpers that read navigate results: the nav error text, title/url from
// the evaluate probe, console lines, the main document and its loader, and
// whether an event belongs to the navigation. Moved verbatim.

use crate::network_interceptor::network_interceptor::{EventAttribution, LoomNetworkEvent};
use ciborium::value::Value as CborValue;
use loom_shared::navigate_outcome::ShimConsoleLine;
use sha2::Digest;

/// Pull a non-empty `errorText` field from a `Page.navigate` CBOR
/// response Map. CDP populates this when navigation fails before a
/// server response (DNS, connection refused, TLS, etc.). Returns
/// `None` if the field is absent or empty (success path).
pub(super) fn extract_nav_error_text(response: &CborValue) -> Option<String> {
    if let CborValue::Map(entries) = response {
        for (k, v) in entries {
            if let (CborValue::Text(key), CborValue::Text(val)) = (k, v) {
                if key == "errorText" && !val.is_empty() {
                    return Some(val.clone());
                }
            }
        }
    }
    None
}

/// Pull `(title, location.href)` out of a `Runtime.evaluate` CBOR
/// response whose `expression` was
/// `JSON.stringify([document.title || '', location.href || ''])`.
///
/// The CDP response shape is:
///   { result: { type: "string", value: "[\"title\",\"url\"]" } }
///
/// Returns `None` when the response doesn't match the expected shape
/// (e.g. CDP error, page hadn't finished load yet, or guard was
/// triggered) — caller falls back to the requested URL + empty title.
pub(super) fn extract_title_and_url_from_evaluate(
    response: &CborValue,
) -> Option<(String, String)> {
    let map = match response {
        CborValue::Map(m) => m,
        _ => return None,
    };
    // Walk to result.value (string carrying the JSON-encoded [title,url] array).
    let result_value = map.iter().find_map(|(k, v)| match k {
        CborValue::Text(s) if s == "result" => Some(v),
        _ => None,
    })?;
    let result_map = match result_value {
        CborValue::Map(m) => m,
        _ => return None,
    };
    let value = result_map.iter().find_map(|(k, v)| match k {
        CborValue::Text(s) if s == "value" => Some(v),
        _ => None,
    })?;
    let json_str = match value {
        CborValue::Text(s) => s,
        _ => return None,
    };
    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let arr = parsed.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let title = arr[0].as_str()?.to_string();
    let final_url = arr[1].as_str()?.to_string();
    Some((title, final_url))
}

/// Extract a `ShimConsoleLine` from a `Runtime.consoleAPICalled` event
/// params. CDP shape:
///   { type: "log"|"warn"|"error"|"info"|..., args: [{type,value}, ...], ... }
///
/// Multi-arg console.log (e.g. `console.log("count:", 42)`) is joined with
/// a single space — matches Chromium devtools' rendering. Non-string
/// args are stringified via the value field as a best-effort (rich
/// inspection is not part of the brief; receipts target agent
/// consumption, not human debugging UX).
pub(super) fn extract_console_line(params: &CborValue) -> Option<ShimConsoleLine> {
    let map = match params {
        CborValue::Map(m) => m,
        _ => return None,
    };
    let level = map
        .iter()
        .find_map(|(k, v)| match (k, v) {
            (CborValue::Text(s), CborValue::Text(val)) if s == "type" => Some(val.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "log".to_string());
    let args = map.iter().find_map(|(k, v)| match k {
        CborValue::Text(s) if s == "args" => Some(v),
        _ => None,
    })?;
    let args_arr = match args {
        CborValue::Array(a) => a,
        _ => return None,
    };
    let parts: Vec<String> = args_arr
        .iter()
        .filter_map(|arg| match arg {
            CborValue::Map(m) => m.iter().find_map(|(k, v)| match (k, v) {
                (CborValue::Text(s), CborValue::Text(val)) if s == "value" => Some(val.clone()),
                (CborValue::Text(s), CborValue::Integer(i)) if s == "value" => {
                    Some(i128::from(*i).to_string())
                }
                (CborValue::Text(s), CborValue::Bool(b)) if s == "value" => Some(b.to_string()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    let message = if parts.is_empty() {
        return None;
    } else {
        parts.join(" ")
    };
    Some(ShimConsoleLine { level, message })
}

/// Find the index of the event attributed to THIS navigation's main
/// document. An event "matches" the navigation when its loaderId equals
/// the `Page.navigate` response's loaderId (preferred — each navigation
/// gets a fresh loader, so late events from a superseded prior load are
/// excluded), else when its frameId equals the navigated (main) frame,
/// else — when neither side carries identifiers — unconditionally
/// (conservative fallback that preserves the pre-attribution failure
/// semantics for harnesses/events without frame plumbing).
///
/// Among matching events, prefer the first carrying an HTTP status
/// (responseReceived) over a transport failure (loadingFailed): chromium
/// pairs ERR_HTTP_RESPONSE_CODE_FAILURE with a 4xx/5xx responseReceived
/// for empty-body responses, and the status is the more actionable
/// verdict (HTTP-first ordering, mirrored host-side in
/// `navigate_execute`).
pub(super) fn find_main_document_index(
    events: &[(LoomNetworkEvent, EventAttribution)],
    nav_frame_id: &str,
    nav_loader_id: &str,
) -> Option<u32> {
    let matching = |attribution: &EventAttribution| {
        attribution_matches_navigation(attribution, nav_frame_id, nav_loader_id)
    };
    let with_status = events
        .iter()
        .position(|(event, attribution)| matching(attribution) && event.status > 0);
    let with_error = events
        .iter()
        .position(|(event, attribution)| matching(attribution) && event.error_reason.is_some());
    let any = events
        .iter()
        .position(|(_, attribution)| matching(attribution));
    with_status.or(with_error).or(any).map(|i| i as u32)
}

/// Whether an event's frame/loader attribution ties it to the navigation
/// identified by `nav_frame_id`/`nav_loader_id`. See
/// `find_main_document_index` for the matching policy.
fn attribution_matches_navigation(
    attribution: &EventAttribution,
    nav_frame_id: &str,
    nav_loader_id: &str,
) -> bool {
    if !attribution.loader_id.is_empty() && !nav_loader_id.is_empty() {
        return attribution.loader_id == nav_loader_id;
    }
    if !attribution.frame_id.is_empty() && !nav_frame_id.is_empty() {
        return attribution.frame_id == nav_frame_id;
    }
    // Unattributed event or navigation without identifiers: treat as
    // main-document so transport failures are never silently dropped.
    true
}

/// Pull `frameId` + `loaderId` from a `Page.navigate` CBOR response Map.
pub(crate) fn extract_frame_loader(response: &CborValue) -> (String, String) {
    let mut frame_id = String::new();
    let mut loader_id = String::new();
    if let CborValue::Map(entries) = response {
        for (k, v) in entries {
            if let (CborValue::Text(key), CborValue::Text(val)) = (k, v) {
                match key.as_str() {
                    "frameId" => frame_id = val.clone(),
                    "loaderId" => loader_id = val.clone(),
                    _ => {}
                }
            }
        }
    }
    (frame_id, loader_id)
}
