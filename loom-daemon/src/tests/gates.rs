use super::*;

// ─── Per-action deadline kill → typed request_timeout on the wire ──────────
// The executor traps an over-deadline action with LoomErrorCode::RequestTimeout
// (see loom-host session_executor). map_loom_error must carry that to the wire
// code `request_timeout` rather than collapsing to the InternalError catch-all
// (which would mask a deliberate deadline kill as an internal fault).
#[test]
fn map_loom_error_preserves_request_timeout() {
    use loom_core::error::{LoomError, LoomErrorCode};
    let e = LoomError::new(
        LoomErrorCode::RequestTimeout,
        "action deadline_ms of 2000 ms exceeded before the action completed".to_string(),
    );
    let wire = map_loom_error(&e);
    assert_eq!(wire, LoomErrorCode::RequestTimeout);
    assert_eq!(wire.as_wire(), "request_timeout");
}

#[test]
fn validate_label_canonical_accepts_valid_labels() {
    for ok in ["gh", "github:token", "my-label_1", &"a".repeat(64)] {
        assert!(
            validate_label_canonical(ok).is_ok(),
            "expected {ok:?} to be accepted"
        );
    }
}

#[test]
fn validate_label_canonical_rejects_invalid_labels() {
    assert!(validate_label_canonical("").is_err(), "empty");
    assert!(
        validate_label_canonical(&"a".repeat(65)).is_err(),
        "over 64 chars"
    );
    for bad in [
        "has space",
        "slash/here",
        "dot.here",
        "emoji😀",
        "tab\there",
    ] {
        assert!(
            validate_label_canonical(bad).is_err(),
            "expected {bad:?} to be rejected"
        );
    }
}

#[test]
fn validate_label_or_rpc_err_maps_rejection_to_invalid_argument() {
    // Valid label passes through.
    assert!(validate_label_or_rpc_err("gh").is_ok());
    // Invalid label maps to the same adapter error as InvalidArgument.
    let err = validate_label_or_rpc_err("bad/label").expect_err("should reject");
    let expected = map_loom_error(&LoomError::new(
        loom_core::error::LoomErrorCode::InvalidArgument,
        "x",
    ));
    assert_eq!(
        err, expected,
        "rejection must map to the same adapter error code as InvalidArgument"
    );
}

/// receipt envelope shape after daemon-side rejection.
/// Pins the wire fields the operator's `loom action web.evaluate`
/// reproducer reads from stdout JSON.
#[test]
fn profile_restricted_evaluate_receipt_carries_required_fields() {
    let receipt = profile_restricted_evaluate_receipt(42, "01HZSESSION", "window.location");
    assert_eq!(receipt.action_id, 42);
    assert_eq!(receipt.session_id, "01HZSESSION");
    assert!(matches!(
        receipt.status,
        loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus::Error
    ));
    let err = receipt.error.expect("error envelope present");
    assert_eq!(err.kind, "profile_restricted");
    let detail = err.detail.expect("detail present");
    assert_eq!(detail["matched_pattern"], "window.location");
    assert_eq!(detail["profile"], "safe");
    assert_eq!(detail["violation"], "safe_profile_evaluate_denylist_match");
    // Tier-2 navigate fields should all be None on a synthesized
    // error receipt — no DOM/screenshot/network on a refused action.
    assert!(receipt.url.is_none());
    assert!(receipt.dom_snapshot_hash.is_none());
    assert!(receipt.network_summary.is_none());
    assert_eq!(receipt.timing_ticks, 0);
}

/// verify the operator's exact reproducer pattern
/// matches the daemon's denylist BEFORE shim dispatch (through
/// `find_denylist_match`, the exact routine the gate calls).
#[test]
fn evaluate_denylist_blocks_operator_reproducer_window_location_assignment() {
    let expr = "window.location.href = \"https://evil.example.com\"";
    let matched = loom_shared::safety::find_denylist_match(expr);
    assert_eq!(matched, Some("window.location"));
}

/// whitespace/comment-smuggled variants hit the same gate — the
/// normalized second pass of `find_denylist_match` (audit
/// 2026-06-10: the gate was raw-substring only).
#[test]
fn evaluate_denylist_blocks_token_separator_smuggling() {
    for expr in [
        "window . location = 'https://evil.example.com'",
        "document/**/.cookie = ''",
        "eval ('alert(1)')",
    ] {
        assert!(
            loom_shared::safety::find_denylist_match(expr).is_some(),
            "smuggled variant must be blocked: {expr:?}"
        );
    }
}

/// service-worker registration is gated; feature detection is allowed.
#[test]
fn evaluate_denylist_gates_service_worker_register_not_feature_detect() {
    let register = "navigator.serviceWorker.register('/sw.js')";
    let detect = "if ('serviceWorker' in navigator) {}";
    assert!(
        loom_shared::safety::find_denylist_match(register).is_some(),
        "registration must be blocked"
    );
    assert!(
        loom_shared::safety::find_denylist_match(detect).is_none(),
        "feature detection must NOT be blocked"
    );
}

/// Wire string — `LoomErrorCode::ProfileRestricted`
/// serializes as `"profile_restricted"`. Mirrors what the receipt's
/// `error.kind` carries; if these drift, the operator's grep on
/// the receipt JSON breaks.
#[test]
fn loom_error_code_profile_restricted_wire_string_matches_receipt_kind() {
    use loom_shared::error_format::LoomErrorCode;
    assert_eq!(
        LoomErrorCode::ProfileRestricted.as_wire(),
        "profile_restricted"
    );
}

/// Each `CookieValidationError` variant maps to a stable snake_case
/// wire string. The operator's `loom action web.set_cookies` failure
/// receipt carries `detail.code = <wire string>` so dashboards can
/// group by validation reason.
#[test]
fn cookie_validation_code_covers_all_variants() {
    use loom_shared::cookie_types::CookieValidationError as E;
    assert_eq!(cookie_validation_code(&E::NameEmpty), "name_empty");
    assert_eq!(
        cookie_validation_code(&E::NameInvalid { ch: ';' }),
        "name_invalid"
    );
    assert_eq!(
        cookie_validation_code(&E::ValueTooLarge { size: 5_000 }),
        "value_too_large"
    );
    assert_eq!(
        cookie_validation_code(&E::InvalidSameSite("foo".to_string())),
        "invalid_same_site"
    );
    assert_eq!(
        cookie_validation_code(&E::InvalidExpires(f64::NAN)),
        "invalid_expires"
    );
    assert_eq!(
        cookie_validation_code(&E::TooManyCookies(65)),
        "too_many_cookies"
    );
}

/// Receipt envelope shape returned when daemon-side validation
/// rejects a `web.set_cookies` payload. Pins the wire fields the
/// CLI reproducer reads (`error.kind` and `error.detail.code`).
#[test]
fn cookie_validation_error_receipt_carries_required_fields() {
    let r = cookie_validation_error_receipt(
        7,
        "01HZSESSION",
        "too_many_cookies",
        "65 cookies provided, max is 64".to_string(),
    );
    assert_eq!(r.action_id, 7);
    assert_eq!(r.session_id, "01HZSESSION");
    assert!(matches!(
        r.status,
        loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus::Error
    ));
    let err = r.error.expect("error envelope present");
    assert_eq!(err.kind, "cookie_validation_error");
    let detail = err.detail.expect("detail present");
    assert_eq!(detail["code"], "too_many_cookies");
    assert_eq!(detail["message"], "65 cookies provided, max is 64");
    // Synthesised error — none of the success-path fields populated.
    assert!(r.set_cookies_result.is_none());
    assert!(r.url.is_none());
    assert!(r.dom_snapshot_hash.is_none());
    assert_eq!(r.timing_ticks, 0);
}

/// `build_chromium_args` defensively emits an empty no-op envelope
/// for a `set_cookies` action whose source is still `grant` by the
/// time it reaches the CDP encoder — the dispatcher should have
/// resolved it upstream, but tests / future callers may bypass
/// that path.
#[test]
fn build_chromium_args_set_cookies_grant_source_emits_empty_no_op() {
    let action = Action::WebSetCookies {
        session_id: s("sess"),
        source: serde_json::json!({
            "source": "grant",
            "grant_id": "grn_abc",
        }),
    };
    let msg = decode_cdp(&action).expect("envelope produced");
    assert_eq!(msg.method, "Network.setCookies");
    // params.cookies = [] (empty array)
    let cookies = params_get(&msg, "cookies").expect("cookies param present");
    match cookies {
        ciborium::value::Value::Array(arr) => assert!(arr.is_empty()),
        other => panic!("expected empty array, got {other:?}"),
    }
}

/// `build_chromium_args` for a resolved inline `set_cookies` passes
/// the cookies array through to the CDP envelope. This is the
/// shape the dispatcher hands `build_chromium_args` after grant
/// resolution.
#[test]
fn build_chromium_args_set_cookies_inline_passes_through_cookies() {
    let action = Action::WebSetCookies {
        session_id: s("sess"),
        source: serde_json::json!({
            "source": "inline",
            "cookies": [
                {"name": "sid", "value": "abc", "domain": "example.com", "path": "/"}
            ],
        }),
    };
    let msg = decode_cdp(&action).expect("envelope produced");
    assert_eq!(msg.method, "Network.setCookies");
    let cookies = params_get(&msg, "cookies").expect("cookies param present");
    match cookies {
        ciborium::value::Value::Array(arr) => {
            assert_eq!(arr.len(), 1);
        }
        other => panic!("expected array, got {other:?}"),
    }
}
