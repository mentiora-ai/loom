use super::*;

/// The trusted-input DISPATCH budget is bounded inside the effective deadline
/// AND stays above real CDP ack latency (tens of ms). The load-bearing
/// regression guard: a common tight deadline (1–3s) must NOT collapse the
/// dispatch recv to ~1ms — that loses the mouse-ack race on real Chrome and
/// reports a normal click as `click_failed`. (It must also never be 0, which
/// would re-collapse the recv to the ~30s floor.)
#[test]
fn interaction_dispatch_budget_leaves_room_for_a_real_ack() {
    use crate::settle_budget::interaction_dispatch_budget_ms;
    // No deadline (and the loom `Some(0)` == no-deadline convention) → base.
    assert_eq!(interaction_dispatch_budget_ms(None), 10_000);
    assert_eq!(interaction_dispatch_budget_ms(Some(0)), 10_000);
    // A common tight deadline must leave a REALISTIC ack budget (≫ tens of ms),
    // not collapse toward 1ms — the ship-review regression. A real CDP ack is
    // several to tens of ms, so require comfortably ≥ several hundred ms.
    for dl in [3_000_u64, 5_000, 10_000] {
        let b = interaction_dispatch_budget_ms(Some(dl));
        assert!(
            b >= 500,
            "deadline_ms={dl} gave dispatch budget {b}ms — too tight for a real \
                 CDP ack; a normal click would fail its mouse-ack race"
        );
        assert!(b <= dl, "dispatch budget must stay inside the deadline");
    }
    // Bounded by the base for large deadlines.
    assert!(interaction_dispatch_budget_ms(Some(50_000)) <= 10_000);
    // Never 0 (would re-collapse the dispatch recv to the ~30s floor).
    assert!(interaction_dispatch_budget_ms(Some(1)) >= 1);
}

/// interactive-settle-bounded: the interaction settle budget is ALWAYS bounded
/// strictly inside the caller's `deadline_ms` (minus headroom) — this is the
/// "a completed click never surfaces an rpc timeout" guarantee at the budget
/// level. Also: no deadline → the fixed navigate-style default (10s), itself
/// well inside the 15s tools/call deadline.
#[test]
fn interaction_settle_budget_is_bounded_inside_deadline() {
    use crate::settle_budget::interaction_settle_budget_ms;
    // No deadline → fixed default, comfortably under the 15s RPC deadline.
    assert_eq!(interaction_settle_budget_ms(None, 0), 10_000);
    // loom convention: `Some(0)` ALSO means "no deadline" → full base budget,
    // NOT a 0ms deadline (ship-council FND-0005/0007).
    assert_eq!(interaction_settle_budget_ms(Some(0), 0), 10_000);
    // Typical loom-mcp deadline (15s): budget stays under it.
    assert!(interaction_settle_budget_ms(Some(15_000), 0) < 15_000);
    // Caller-tighter-than-default deadline: budget clamps BELOW it.
    assert!(
        interaction_settle_budget_ms(Some(5_000), 0) < 5_000,
        "budget must clamp below a tight deadline so the receipt beats it"
    );
    // Elapsed input-dispatch time is subtracted from the remaining budget.
    // Headroom is now 3s (reserves room for the shim's post-settle resume
    // cleanup, click-cross-origin-until ship FND-0001), so a 5s deadline with
    // 3s already spent dispatching leaves 0 (5000-3000-3000 saturates) — out
    // of time, an immediate typed timeout, NOT a floored window that would
    // overrun the deadline (ship FND-0009).
    assert_eq!(interaction_settle_budget_ms(Some(5_000), 3_000), 0);
    // Pathologically tiny deadline: no lower floor — a deadline smaller than
    // the headroom yields 0 (return fast) rather than being floored UP past
    // the caller's own deadline (ship FND-0009).
    assert_eq!(interaction_settle_budget_ms(Some(200), 0), 0);
    // Deadline already blown by a slow dispatch: 0, never a floored window.
    assert_eq!(interaction_settle_budget_ms(Some(1_000), 5_000), 0);
    // A deadline with room to spare past the headroom yields a real window.
    assert_eq!(interaction_settle_budget_ms(Some(6_000), 0), 3_000); // 6000-3000
}

/// interactive-settle-bounded determinism (NFR-DET-01): folding the settle
/// verdict onto an input receipt stamps ONLY the observational
/// `settle_until`/`settle_outcome` fields and leaves `outcome_hash` /
/// `action_hash` byte-identical — so replay stays structural.
#[test]
fn stamp_settle_outcome_leaves_hashes_untouched() {
    use crate::wire_receipts::{build_input_dispatch_receipt, stamp_settle_outcome};
    use loom_host::shim_manager::InputDispatchOutcome;
    let action = Action::WebClick {
        session_id: s("sess"),
        selector: s("#go"),
        until: Some(s("settled")),
    };
    let mut receipt = build_input_dispatch_receipt(7, "sess", &action, InputDispatchOutcome::Ok);
    let hash_before = receipt.outcome_hash.clone();
    let action_hash_before = receipt.action_hash.clone();
    assert!(receipt.settle_outcome.is_none(), "no settle stamped yet");

    let verdict = loom_shared::navigate_outcome::WaitOutcome {
        settle_until: s("settled"),
        settle_outcome: s("dom_unstable"),
        settle_ms: 10_000,
        network_count_at_settle: 1,
    };
    stamp_settle_outcome(&mut receipt, &verdict);

    assert_eq!(receipt.settle_until.as_deref(), Some("settled"));
    assert_eq!(receipt.settle_outcome.as_deref(), Some("dom_unstable"));
    assert_eq!(
        receipt.outcome_hash, hash_before,
        "settle verdict must NOT change the constant outcome_hash marker"
    );
    assert_eq!(
        receipt.action_hash, action_hash_before,
        "settle verdict must NOT change the session-independent action_hash"
    );
}

/// web.wait is now host-intercepted (poll `host.wait` → `send_wait`, reusing the
/// locator-grammar resolver), so build_chromium_args returns None — the guard
/// against regressing back to the raw `querySelector(sel)` guest envelope that
/// threw `js_throw` on `text=`/`role=` locators (the reported bug).
#[test]
fn build_chromium_args_wait_is_host_side_returns_none() {
    let action = Action::WebWait {
        session_id: s("sess"),
        selector: s("text=Ready 1"),
        timeout_ms: Some(5000),
    };
    assert!(
        decode_cdp(&action).is_none(),
        "web.wait is host-side (polled locator resolution) → expected None"
    );
}

/// The host-side `web.wait` receipt mirrors the click contract: a `Resolved`
/// wait carries a constant `outcome_hash` marker + a SESSION-INDEPENDENT
/// `action_hash` (replay-equal), and a `PredicateFalse` wait surfaces the typed
/// `wait_predicate_false` error kind. `timeout_ms` is excluded from the hash.
#[test]
fn build_wait_receipt_is_session_independent_and_typed() {
    use crate::wire_receipts::build_wait_receipt;
    use loom_host::shim_manager::WaitResolveOutcome;
    let a_sess_a = Action::WebWait {
        session_id: s("sess-A"),
        selector: s("text=Ready 1"),
        timeout_ms: Some(5000),
    };
    let a_sess_b = Action::WebWait {
        session_id: s("sess-B"),
        selector: s("text=Ready 1"),
        timeout_ms: Some(9999), // different timeout → must NOT change the hash
    };
    let r_a = build_wait_receipt(1, "sess-A", &a_sess_a, WaitResolveOutcome::Resolved);
    let r_b = build_wait_receipt(2, "sess-B", &a_sess_b, WaitResolveOutcome::Resolved);
    assert!(
        r_a.action_hash.is_some(),
        "host-side wait receipt must carry action_hash"
    );
    assert!(
        r_a.outcome_hash.is_some(),
        "resolved wait must stamp the constant dispatch marker"
    );
    assert_eq!(
        r_a.action_hash, r_b.action_hash,
        "action_hash must be session- and timeout-independent (replay-equal)"
    );
    // Different selector → different action_hash.
    let a_other = Action::WebWait {
        session_id: s("sess-A"),
        selector: s("text=Other"),
        timeout_ms: Some(5000),
    };
    let r_other = build_wait_receipt(3, "sess-A", &a_other, WaitResolveOutcome::Resolved);
    assert_ne!(r_a.action_hash, r_other.action_hash);

    // PredicateFalse → typed wait_predicate_false error receipt, no outcome marker.
    let r_to = build_wait_receipt(4, "sess-A", &a_sess_a, WaitResolveOutcome::PredicateFalse);
    let err = r_to.error.expect("timeout must produce an error receipt");
    assert_eq!(err.kind, "wait_predicate_false");
}

/// `mode:"value"` sets value via the framework-aware native setter (so
/// React/Vue/Angular trackers see the change) AND dispatches input/change
/// events. (The default `fill` mode is host-side CDP Input.insertText and
/// builds no Runtime.evaluate args — covered by the host-side fill tests.)
#[test]
fn build_chromium_args_type_emits_runtime_evaluate_setting_value_and_dispatching_input_change() {
    let action = Action::WebType {
        session_id: s("sess"),
        selector: s("input"),
        text: s("hello"),
        mode: Some(s("value")),
        until: None,
    };
    let msg = decode_cdp(&action).expect("Some");
    assert_eq!(msg.method, "Runtime.evaluate");
    let expr = expr_of(&msg);
    // Framework-aware: must call the prototype's value setter, not assign
    // `.value =` directly (which bypasses React's tracker).
    assert!(
        expr.contains("setter.call(el,"),
        "expected setter.call(el, ...) in {expr}"
    );
    assert!(
        expr.contains("HTMLInputElement.prototype"),
        "expected HTMLInputElement.prototype in {expr}"
    );
    assert!(
        !expr.contains(";el.value="),
        "regression: direct el.value= bypasses React tracker, in {expr}"
    );
    assert!(
        expr.contains("new Event('input'"),
        "expected input event in {expr}"
    );
    assert!(
        expr.contains("new Event('change'"),
        "expected change event in {expr}"
    );
}

/// screenshot uses Page.captureScreenshot { format: "png" }.
#[test]
fn build_chromium_args_screenshot_emits_page_capture_screenshot_png() {
    let action = Action::WebScreenshot {
        session_id: s("sess"),
        selector: None,
    };
    let msg = decode_cdp(&action).expect("Some");
    assert_eq!(msg.method, "Page.captureScreenshot");
    match params_get(&msg, "format").expect("format param") {
        ciborium::value::Value::Text(t) => assert_eq!(t, "png"),
        other => panic!("format not text: {other:?}"),
    }
}

#[test]
fn build_wire_receipt_error_shim_failure_with_typed_http_status_detail() {
    let detail = r#"{"kind":"http_status","url":"http://fake.test/status/404","status_code":404}"#;
    let err = build_wire_receipt_error("shim-failure", Some(detail));
    assert_eq!(err.kind, "http_status");
    let d = err.detail.as_ref().expect("detail must be present");
    assert_eq!(
        d.get("url").and_then(|v| v.as_str()),
        Some("http://fake.test/status/404")
    );
    assert_eq!(d.get("status_code").and_then(|v| v.as_u64()), Some(404));
    // `kind` must NOT be in detail — it's been hoisted to the wire kind field.
    assert!(
        d.get("kind").is_none(),
        "kind should be hoisted, not in detail"
    );
}

#[test]
fn build_wire_receipt_error_shim_failure_with_typed_dns_failure_detail() {
    let detail = r#"{"kind":"dns_failure","url":"http://fake.test/error/x","chromium_error":"net::ERR_NAME_NOT_RESOLVED"}"#;
    let err = build_wire_receipt_error("shim-failure", Some(detail));
    assert_eq!(err.kind, "dns_failure");
    let d = err.detail.as_ref().expect("detail must be present");
    assert_eq!(
        d.get("chromium_error").and_then(|v| v.as_str()),
        Some("net::ERR_NAME_NOT_RESOLVED")
    );
}

#[test]
fn build_wire_receipt_error_untyped_shim_failure_falls_back_to_message() {
    // Plain-string shim-failure (not structured JSON) — the raw string
    // becomes detail.message; kind keeps the raw error_code.
    let err = build_wire_receipt_error("shim-failure", Some("chromium subprocess died"));
    assert_eq!(err.kind, "shim-failure");
    let d = err.detail.as_ref().expect("detail must be present");
    assert_eq!(
        d.get("message").and_then(|v| v.as_str()),
        Some("chromium subprocess died")
    );
}

#[test]
fn build_wire_receipt_error_non_shim_failure_uses_code_as_kind() {
    let err = build_wire_receipt_error("budget-exceeded", Some("navigate exceeded 30s"));
    assert_eq!(err.kind, "budget-exceeded");
    let d = err.detail.as_ref().expect("detail must be present");
    assert_eq!(
        d.get("message").and_then(|v| v.as_str()),
        Some("navigate exceeded 30s")
    );
}

#[test]
fn build_wire_receipt_error_empty_details_yields_no_detail() {
    let err = build_wire_receipt_error("internal", None);
    assert_eq!(err.kind, "internal");
    assert!(err.detail.is_none());
    let err2 = build_wire_receipt_error("internal", Some(""));
    assert!(err2.detail.is_none());
}

/// Security: selector strings containing JS metacharacters must be
/// JSON-escaped wherever they're interpolated into Runtime.evaluate JS.
/// cdp-trusted-input: web.click is now host-side (the selector flows as a
/// CBOR CDP `DOM.querySelector` param — no JS-injection surface), so this
/// pins the remaining JS-interpolating path: web.type in `value` mode.
#[test]
fn build_chromium_args_value_type_json_escapes_selector_with_double_quote() {
    // selector contains a literal double-quote character: a[id="x']
    let selector = "a[id=\"x']".to_string();
    let action = Action::WebType {
        session_id: s("sess"),
        selector: selector.clone(),
        text: s("v"),
        mode: Some(s("value")),
        until: None,
    };
    let msg = decode_cdp(&action).expect("Some");
    let expr = expr_of(&msg);
    // Must contain the JSON-escaped form: "a[id=\"x']"
    assert!(
        expr.contains("\"a[id=\\\"x']\""),
        "selector not JSON-escaped; expr was: {expr}"
    );
    // Must NOT contain a raw unescaped double-quote inside the literal that
    // would have closed the JS string early.
    // Validate by parsing the expression: it should still be a syntactically
    // closeable JS source — at minimum, count of unescaped double-quotes
    // should be even (open+close pairs).
    let mut escaped = false;
    let mut quotes = 0usize;
    for ch in expr.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => quotes += 1,
            _ => {}
        }
    }
    assert_eq!(
        quotes % 2,
        0,
        "odd number of unescaped quotes in expr: {expr}"
    );
}

/// cdp-trusted-input: the DEFAULT (`mode:None`) and explicit `mode:"fill"`
/// are dispatched host-side (CDP Input.insertText), so `build_chromium_args`
/// builds NO Runtime.evaluate args for them — proving the default flip is
/// routed away from the WASM-guest value path. `mode:"keystrokes"` is also
/// host-side; only `value` (and unknown → value) builds guest args.
#[test]
fn build_chromium_args_type_default_and_fill_are_host_side_no_guest_args() {
    for mode in [None, Some(s("fill")), Some(s("keystrokes"))] {
        let action = Action::WebType {
            session_id: s("sess"),
            selector: s("input"),
            text: s("hello"),
            mode: mode.clone(),
            until: None,
        };
        assert!(
            decode_cdp(&action).is_none(),
            "web.type mode {mode:?} must be host-side intercepted (no Runtime.evaluate args)"
        );
    }
    // value (and an unknown string) DO build guest args.
    for mode in [Some(s("value")), Some(s("totally-unknown"))] {
        let action = Action::WebType {
            session_id: s("sess"),
            selector: s("input"),
            text: s("hello"),
            mode,
            until: None,
        };
        let msg = decode_cdp(&action).expect("value/unknown mode builds guest args");
        assert_eq!(msg.method, "Runtime.evaluate");
    }
}

/// The single-source-of-truth mode classifier (decisions.md D8).
#[test]
fn classify_web_type_mode_maps_modes_to_dispatch_paths() {
    use crate::guest_args::{classify_web_type_mode, WebTypeDispatch};
    assert_eq!(classify_web_type_mode(None), WebTypeDispatch::Fill);
    assert_eq!(classify_web_type_mode(Some("fill")), WebTypeDispatch::Fill);
    assert_eq!(
        classify_web_type_mode(Some("keystrokes")),
        WebTypeDispatch::Keystrokes
    );
    assert_eq!(
        classify_web_type_mode(Some("value")),
        WebTypeDispatch::ValueGuest
    );
    // Unknown strings fall back to value (back-compat — no error).
    assert_eq!(
        classify_web_type_mode(Some("nope")),
        WebTypeDispatch::ValueGuest
    );
}
