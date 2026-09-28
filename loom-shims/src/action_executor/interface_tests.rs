// Interface tests for `ActionExecutor`.
// Verifies cdp_send async signature,
// R3 pre-condition, R1 pre-condition,
// no CDP payload escape (`ActionResult` is typed), error mapping.

use super::action_executor::{
    action_error_to_response, parse_navigate_budget, should_resume_virtual_clock, ActionError,
    ActionResult, DEFAULT_ACTION_BUDGET, DEFAULT_NAVIGATE_BUDGET,
};
use crate::ipc_endpoint::ipc_endpoint::{ShimErrorCode, ShimResponse};
use ciborium::value::Value as CborValue;
use std::time::Duration;

// === Default action budget ===

#[test]
fn default_action_budget_is_thirty_seconds() {
    assert_eq!(DEFAULT_ACTION_BUDGET, Duration::from_secs(30));
}

// === Navigate budget knob (LOOM_SHIM_CDP_TIMEOUT_MS) ===
// The per-CDP-command navigate budget is configurable; on expiry it surfaces
// typed (CdpTimeout → ShimTimeout → wire `shim_timeout`), distinct from a
// broken page. These pin the parse policy (the cached env read is process-
// global, so the parse is factored pure for deterministic in-process testing;
// the wire-level expiry behaviour is covered by the fake-chromium e2e
// `integration_navigate_timeout_budget`).

#[test]
fn default_navigate_budget_is_ten_seconds() {
    // Regression guard: the documented default must stay 10s so an absent knob
    // keeps the pre-feature behaviour exactly.
    assert_eq!(DEFAULT_NAVIGATE_BUDGET, Duration::from_secs(10));
}

#[test]
fn navigate_budget_unset_falls_back_to_default() {
    assert_eq!(parse_navigate_budget(None), DEFAULT_NAVIGATE_BUDGET);
}

#[test]
fn navigate_budget_parses_explicit_millis() {
    // 30000ms is the acceptance "raise it so a slow navigate succeeds" value.
    assert_eq!(
        parse_navigate_budget(Some("30000")),
        Duration::from_secs(30)
    );
    assert_eq!(
        parse_navigate_budget(Some("1500")),
        Duration::from_millis(1500)
    );
}

#[test]
fn navigate_budget_rejects_zero_and_garbage_to_default() {
    // A `0` or non-numeric knob must NOT disable the budget — that would
    // re-open the unbounded-navigate trap this guards against.
    assert_eq!(parse_navigate_budget(Some("0")), DEFAULT_NAVIGATE_BUDGET);
    assert_eq!(parse_navigate_budget(Some("")), DEFAULT_NAVIGATE_BUDGET);
    assert_eq!(parse_navigate_budget(Some("abc")), DEFAULT_NAVIGATE_BUDGET);
    assert_eq!(parse_navigate_budget(Some("-5")), DEFAULT_NAVIGATE_BUDGET);
    assert_eq!(parse_navigate_budget(Some("10.5")), DEFAULT_NAVIGATE_BUDGET);
}

#[test]
fn cdp_timeout_is_distinct_from_broken_page_codes() {
    // The typed timeout (→ LoomErrorCode::ShimTimeout) must be distinguishable
    // from a broken page, which surfaces as a SurfaceTrap-family shim code.
    assert_ne!(ShimErrorCode::CdpTimeout, ShimErrorCode::CdpProtocolError);
    assert_ne!(
        ShimErrorCode::CdpTimeout,
        ShimErrorCode::ChromiumUnavailable
    );
    assert_ne!(ShimErrorCode::CdpTimeout, ShimErrorCode::ShimInternalError);
}

// === ActionResult is typed, no raw CDP bytes for navigate ===

#[test]
fn navigated_result_carries_typed_fields_not_cdp_bytes() {
    let r = ActionResult::Navigated {
        target_id: 7,
        frame_id: "frame-1".into(),
        loader_id: "loader-1".into(),
        network_events: vec![],
        dom_after_sha256: "a".repeat(64),
        screenshot_sha256: "b".repeat(64),
        url: "https://example.com/".into(),
        final_url: "https://example.com/".into(),
        page_title: String::new(),
        status_code: 200,
        main_document_event_index: None,
        dom_bytes: vec![],
        screenshot_bytes: vec![],
        console_lines: vec![],
        blocked_events: vec![],
        network_entries: vec![],
        network_entries_truncated: false,
        settle_until: "settled".into(),
        settle_outcome: "reached".into(),
        settle_ms: 0,
        network_count_at_settle: 0,
    };
    match r {
        ActionResult::Navigated {
            target_id,
            frame_id,
            loader_id,
            network_events,
            dom_after_sha256,
            screenshot_sha256,
            ..
        } => {
            assert_eq!(target_id, 7);
            assert_eq!(frame_id, "frame-1");
            assert_eq!(loader_id, "loader-1");
            assert_eq!(network_events.len(), 0);
            assert_eq!(dom_after_sha256.len(), 64);
            assert_eq!(screenshot_sha256.len(), 64);
        }
        _ => panic!("variant mismatch"),
    }
}

#[test]
fn cdp_send_result_passes_cbor_through_only_for_cdp_send_path() {
    // The opaque CBOR pass-through is intentional ONLY for the
    // `cdp_send` path; `Navigated` and `PageClosed` use typed fields.
    let r = ActionResult::CdpResult {
        result: CborValue::Integer(42i64.into()),
    };
    if let ActionResult::CdpResult { result } = r {
        assert_eq!(result, CborValue::Integer(42i64.into()));
    }
}

// === ActionError → ShimErrorCode mapping ===

#[test]
fn r3_violation_maps_to_shim_internal_error() {
    let code: ShimErrorCode = ActionError::R3OrderingViolation(7).into();
    assert_eq!(code, ShimErrorCode::ShimInternalError);
}

#[test]
fn target_unknown_maps_to_target_unknown_code() {
    let code: ShimErrorCode = ActionError::TargetUnknown(99).into();
    assert_eq!(code, ShimErrorCode::TargetUnknown);
}

#[test]
fn cdp_error_inside_action_error_maps_through() {
    use crate::cdp_connection::cdp_connection::CdpError;
    let code: ShimErrorCode = ActionError::Cdp(CdpError::Timeout { ms: 50 }).into();
    assert_eq!(code, ShimErrorCode::CdpTimeout);
}

// === Error envelope helper produces stable shape ===

#[test]
fn action_error_to_response_produces_error_envelope() {
    let resp = action_error_to_response(ActionError::TargetUnknown(7), 50, Some(42));
    match resp {
        ShimResponse::Error {
            request_id,
            session_id,
            code,
            detail,
        } => {
            assert_eq!(request_id, 50);
            assert_eq!(session_id, Some(42));
            assert_eq!(code, ShimErrorCode::TargetUnknown);
            assert!(detail.contains("7"), "detail = {}", detail);
        }
        _ => panic!("expected Error envelope"),
    }
}

#[test]
fn action_error_to_response_works_without_session_id() {
    let resp = action_error_to_response(ActionError::R3OrderingViolation(1), 60, None);
    if let ShimResponse::Error {
        request_id,
        session_id,
        ..
    } = resp
    {
        assert_eq!(request_id, 60);
        assert_eq!(session_id, None);
    } else {
        panic!("expected Error envelope");
    }
}

// === Fix 2 (navigate-status-code-error-surfacing) ===
// extract_nav_error_text pulls CDP `Page.navigate`'s non-empty `errorText`
// field — the channel for DNS / conn-refused / TLS / etc.

#[test]
fn extract_nav_error_text_returns_some_when_error_text_is_non_empty() {
    use super::page_extract::extract_nav_error_text;
    let response = CborValue::Map(vec![
        (
            CborValue::Text("frameId".into()),
            CborValue::Text("frame-1".into()),
        ),
        (
            CborValue::Text("errorText".into()),
            CborValue::Text("net::ERR_NAME_NOT_RESOLVED".into()),
        ),
    ]);
    assert_eq!(
        extract_nav_error_text(&response),
        Some("net::ERR_NAME_NOT_RESOLVED".to_string())
    );
}

#[test]
fn extract_nav_error_text_returns_none_when_field_absent() {
    use super::page_extract::extract_nav_error_text;
    let response = CborValue::Map(vec![
        (
            CborValue::Text("frameId".into()),
            CborValue::Text("frame-1".into()),
        ),
        (
            CborValue::Text("loaderId".into()),
            CborValue::Text("loader-1".into()),
        ),
    ]);
    assert_eq!(extract_nav_error_text(&response), None);
}

#[test]
fn extract_nav_error_text_returns_none_when_field_empty() {
    use super::page_extract::extract_nav_error_text;
    // CDP returns an empty errorText on success — must not synthesize an
    // error event.
    let response = CborValue::Map(vec![(
        CborValue::Text("errorText".into()),
        CborValue::Text("".into()),
    )]);
    assert_eq!(extract_nav_error_text(&response), None);
}

// === Main-document identification (find_main_document_index) ===
// The navigate failure verdict must be scoped to THIS navigation's main
// document: an iframe's 404/transport failure must not fail the whole
// navigate, and a stale event from a superseded prior load (different
// loaderId) must be excluded.

use crate::network_interceptor::network_interceptor::{EventAttribution, LoomNetworkEvent};

fn doc_event(status: u16, error_reason: Option<&str>) -> LoomNetworkEvent {
    LoomNetworkEvent {
        method: String::new(),
        url: "http://fake.test/".into(),
        request_hash: String::new(),
        response_hash: String::new(),
        status,
        content_type: String::new(),
        duration_ms: 0,
        response_bytes: 0,
        error_reason: error_reason.map(String::from),
        error_kind: error_reason.map(|_| "network_error".to_string()),
    }
}

fn attribution(frame_id: &str, loader_id: &str) -> EventAttribution {
    EventAttribution {
        request_id: "R".into(),
        frame_id: frame_id.into(),
        loader_id: loader_id.into(),
    }
}

#[test]
fn main_document_index_excludes_iframe_404_by_loader() {
    use super::page_extract::find_main_document_index;
    // Iframe document 404 first (different loader), main 200 second —
    // the verdict must point at the main document, not the iframe.
    let events = vec![
        (
            doc_event(404, None),
            attribution("frame-iframe", "loader-iframe"),
        ),
        (
            doc_event(200, None),
            attribution("frame-main", "loader-main"),
        ),
    ];
    assert_eq!(
        find_main_document_index(&events, "frame-main", "loader-main"),
        Some(1)
    );
}

#[test]
fn main_document_index_none_when_only_iframe_events() {
    use super::page_extract::find_main_document_index;
    let events = vec![(
        doc_event(404, None),
        attribution("frame-iframe", "loader-iframe"),
    )];
    assert_eq!(
        find_main_document_index(&events, "frame-main", "loader-main"),
        None,
        "iframe-only events must not be promoted to main-document verdict"
    );
}

#[test]
fn main_document_index_excludes_iframe_transport_failure() {
    use super::page_extract::find_main_document_index;
    // A blocklist-gated iframe document (Fetch.failRequest →
    // loadingFailed, attributed via requestWillBeSent backfill) must
    // not fail the navigate.
    let events = vec![
        (
            doc_event(0, Some("net::ERR_BLOCKED_BY_CLIENT")),
            attribution("frame-iframe", "loader-iframe"),
        ),
        (
            doc_event(200, None),
            attribution("frame-main", "loader-main"),
        ),
    ];
    assert_eq!(
        find_main_document_index(&events, "frame-main", "loader-main"),
        Some(1)
    );
}

#[test]
fn main_document_index_prefers_http_status_over_paired_transport_error() {
    use super::page_extract::find_main_document_index;
    // Chromium pairs ERR_HTTP_RESPONSE_CODE_FAILURE with the 4xx
    // responseReceived for empty-body responses; HTTP-first ordering
    // must pick the status-bearing event even when the failure event
    // is drained first.
    let events = vec![
        (
            doc_event(0, Some("net::ERR_HTTP_RESPONSE_CODE_FAILURE")),
            attribution("frame-main", "loader-main"),
        ),
        (
            doc_event(404, None),
            attribution("frame-main", "loader-main"),
        ),
    ];
    assert_eq!(
        find_main_document_index(&events, "frame-main", "loader-main"),
        Some(1)
    );
}

#[test]
fn main_document_index_falls_back_to_frame_match_without_loaders() {
    use super::page_extract::find_main_document_index;
    let events = vec![
        (doc_event(404, None), attribution("frame-iframe", "")),
        (doc_event(200, None), attribution("frame-main", "")),
    ];
    assert_eq!(
        find_main_document_index(&events, "frame-main", ""),
        Some(1),
        "frameId matching must apply when loader ids are unavailable"
    );
}

#[test]
fn main_document_index_treats_unattributed_events_as_main() {
    use super::page_extract::find_main_document_index;
    // Harnesses (and synthetic Page.navigate-errorText events) carry no
    // frame/loader ids; conservative fallback keeps the pre-attribution
    // failure semantics so transport failures are never dropped.
    let events = vec![(
        doc_event(0, Some("net::ERR_NAME_NOT_RESOLVED")),
        EventAttribution::default(),
    )];
    assert_eq!(
        find_main_document_index(&events, "frame-main", "loader-main"),
        Some(0)
    );
}

#[test]
fn main_document_index_excludes_stale_prior_load_by_loader() {
    use super::page_extract::find_main_document_index;
    // A late event from a superseded prior load keeps its ORIGINAL
    // loaderId; loader-first matching excludes it even though the
    // frameId equals the (persistent) main frame.
    let events = vec![
        (
            doc_event(404, None),
            attribution("frame-main", "loader-old"),
        ),
        (
            doc_event(200, None),
            attribution("frame-main", "loader-new"),
        ),
    ];
    assert_eq!(
        find_main_document_index(&events, "frame-main", "loader-new"),
        Some(1)
    );
}

#[test]
fn main_document_index_none_when_no_events() {
    use super::page_extract::find_main_document_index;
    assert_eq!(
        find_main_document_index(&[], "frame-main", "loader-main"),
        None
    );
}

// === Virtual-clock resume policy (navigate-degradation regression) ===
//
// `should_resume_virtual_clock` decides whether `page_navigate`'s exit guards
// un-pause the renderer's virtual clock. The clock is left FROZEN at the drained
// budget horizon ONLY on the determinism-pinned clean-drain path (replay-equal
// clock reads). In every other case it MUST resume — a frozen clock defers the
// NEXT navigate's Page.loadEventFired, and the determinism-OFF navigate path
// awaits load BEFORE re-arming a budget, so it deadlocks and burns the full
// navigate budget every call (the +20s-per-navigate wedge). All three exit guards
// (success + the two CDP-error bails) route through this one helper.

#[test]
fn resume_clock_left_frozen_only_on_determinism_pinned_clean_drain() {
    // determinism pinned + budget cleanly drained → leave frozen (replay-equal).
    assert!(
        !should_resume_virtual_clock(true, true, true),
        "determinism-pinned clean drain must stay frozen for replay-equality"
    );
}

#[test]
fn resume_clock_under_no_determinism_after_clean_drain_is_the_regression_fix() {
    // THE FIX: determinism OFF (clock_pinned=false) + budget drained. Pre-fix this
    // was `!budget_drained` → false → clock left paused → next navigate's load
    // deferred → +20s/call degradation. Post-fix it MUST resume.
    assert!(
        should_resume_virtual_clock(true, false, true),
        "navigate-degradation: with determinism OFF the clock must resume even after a clean budget drain"
    );
}

#[test]
fn resume_clock_when_budget_not_drained_regardless_of_pin() {
    // A not-cleanly-drained budget leaves the renderer paused mid-flight and would
    // wedge the next command — always resume (unchanged from the original guard).
    assert!(should_resume_virtual_clock(true, true, false));
    assert!(should_resume_virtual_clock(true, false, false));
}

#[test]
fn resume_clock_noop_when_virtual_time_disabled() {
    // vt_active=false (LOOM_CAPTURE_VIRTUAL_TIME=0) → no virtual clock to resume.
    assert!(!should_resume_virtual_clock(false, false, false));
    assert!(!should_resume_virtual_clock(false, true, true));
    assert!(!should_resume_virtual_clock(false, false, true));
    assert!(!should_resume_virtual_clock(false, true, false));
}
