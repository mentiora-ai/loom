use super::*;

/// every Web.* variant produces a decodable CdpMessage.
#[test]
fn build_chromium_args_emits_valid_cdp_message_for_each_web_verb() {
    let session = s("sess-1");
    let cases: Vec<(Action, &str)> = vec![
        (
            Action::WebNavigate {
                session_id: session.clone(),
                url: s("https://example.com"),
                until: None,
                timeout_ms: None,
            },
            "Page.navigate",
        ),
        // cdp-trusted-input: web.click is now host-side (trusted CDP
        // Input.dispatchMouseEvent) → build_chromium_args returns None, so
        // it is not part of this "each verb yields a CdpMessage" sweep.
        (
            Action::WebEvaluate {
                session_id: session.clone(),
                expression: s("1+1"),
            },
            "Runtime.evaluate",
        ),
        (
            // cdp-trusted-input: the DEFAULT (`mode:None`) is now `fill`
            // (host-side CDP Input.insertText) → build_chromium_args returns
            // None; only the legacy `value` mode builds the Runtime.evaluate
            // setter JS, so this sweep uses `mode:"value"` explicitly.
            Action::WebType {
                session_id: session.clone(),
                selector: s("input"),
                text: s("hello"),
                mode: Some(s("value")),
                until: None,
            },
            "Runtime.evaluate",
        ),
        (
            Action::WebSelect {
                session_id: session.clone(),
                selector: s("select"),
                value: s("v1"),
            },
            "Runtime.evaluate",
        ),
        (
            Action::WebHover {
                session_id: session.clone(),
                selector: s("a"),
            },
            "Runtime.evaluate",
        ),
        (
            Action::WebScroll {
                session_id: session.clone(),
                selector: Some(s("body")),
                delta_x: Some(0),
                delta_y: Some(100),
            },
            "Runtime.evaluate",
        ),
        // web.wait is host-intercepted (like web.click) → no guest envelope;
        // its None case is asserted by build_chromium_args_wait_is_host_side_returns_none.
        (
            Action::WebScreenshot {
                session_id: session.clone(),
                selector: None,
            },
            "Page.captureScreenshot",
        ),
        (
            Action::WebSnapshot {
                session_id: session.clone(),
            },
            "DOM.getDocument",
        ),
    ];
    for (action, expected_method) in cases {
        let msg = decode_cdp(&action)
            .unwrap_or_else(|| panic!("build_chromium_args returned None for {action:?}"));
        assert_eq!(msg.method, expected_method, "wrong method for {action:?}");
    }
}

/// web.snapshot must capture shadow DOM + iframe content (`pierce:true`),
/// matching web.navigate (shim STEP 5, already pierce:true) so the two DOM
/// captures hash a comparable node set. Locks the unified contract against
/// silent regression — see specs/2026-06-09-unify-pierce-setting/plan.md.
#[test]
fn build_chromium_args_snapshot_emits_pierce_true() {
    let action = Action::WebSnapshot {
        session_id: s("sess-1"),
    };
    let msg = decode_cdp(&action).expect("Some");
    assert_eq!(msg.method, "DOM.getDocument");
    match params_get(&msg, "pierce").expect("pierce param") {
        ciborium::value::Value::Bool(b) => {
            assert!(*b, "snapshot DOM.getDocument must use pierce:true")
        }
        other => panic!("pierce not a Bool: {other:?}"),
    }
}

/// evaluate carries the user expression verbatim.
#[test]
fn build_chromium_args_evaluate_emits_runtime_evaluate_with_expression() {
    let action = Action::WebEvaluate {
        session_id: s("sess"),
        expression: s("1+1"),
    };
    let msg = decode_cdp(&action).expect("Some");
    assert_eq!(msg.method, "Runtime.evaluate");
    assert_eq!(expr_of(&msg), "1+1");
}

/// `--selector body` must target the document scrolling box
/// (`document.scrollingElement`), NOT `body.scrollBy` (a no-op on standard
/// pages). The `el === document.body` guard routes body → scrollingElement.
#[test]
fn build_scroll_expression_targets_scrolling_element_for_body() {
    let js = build_scroll_expression(&Some(s("body")), 0, 1400);
    assert!(
        js.contains("document.scrollingElement"),
        "body scroll must target scrollingElement: {js}"
    );
    assert!(
        js.contains("document.body"),
        "must guard el === document.body: {js}"
    );
    // returns the post-scroll viewport position
    assert!(js.contains("window.scrollX") && js.contains("window.scrollY"));
    assert!(js.contains("scrollBy(0,1400)"));
}

/// No selector (the new default) → `null`, so the box resolves to
/// `document.scrollingElement` and the page viewport scrolls.
#[test]
fn build_scroll_expression_null_selector_falls_back_to_scrolling_element() {
    let js = build_scroll_expression(&None, 0, 800);
    // selector is embedded as the JS literal `null`
    assert!(
        js.contains("const el=null?"),
        "absent selector must embed as null: {js}"
    );
    assert!(js.contains("document.scrollingElement"));
    assert!(js.contains("scrollBy(0,800)"));
}

/// A real CSS selector is embedded as its querySelector argument; the box
/// falls through to the resolved element (not scrollingElement).
#[test]
fn build_scroll_expression_uses_resolved_selector_for_real_css() {
    let js = build_scroll_expression(&Some(s(".feed")), 10, 0);
    assert!(
        js.contains(r#"document.querySelector(".feed")"#),
        "must query the real selector: {js}"
    );
    assert!(js.contains("scrollBy(10,0)"));
}

/// Injection guard: a selector containing `"` is JSON-escaped via
/// `serde_json::to_string`, so it cannot break out of the JS string literal.
#[test]
fn build_scroll_expression_quotes_selector_with_double_quote() {
    let js = build_scroll_expression(&Some(s(r#"a"b"#)), 0, 0);
    // exact escaped form: the JS string literal "a\"b"
    assert!(
        js.contains(r#""a\"b""#),
        "double-quote selector must be JSON-escaped: {js}"
    );
    // and never the unescaped break-out
    assert!(!js.contains(r#"querySelector(a"b)"#));
}

/// Guest verbs keep a plain CSS selector's payload byte for byte (so the
/// action_hash of every recording that already worked is unchanged), and send
/// the locator grammar — which used to throw inside querySelector — through
/// the shared resolver.
#[test]
fn guest_element_lookup_keeps_plain_css_and_resolves_locators() {
    assert_eq!(
        element_expr("#email").as_deref(),
        Some(r##"document.querySelector("#email")"##)
    );
    assert_eq!(
        build_scroll_expression(&Some(s(".feed")), 10, 0),
        r#"(()=>{const el=".feed"?document.querySelector(".feed"):null;const box=(!el||el===document.body||el===document.documentElement)?(document.scrollingElement||document.documentElement):el;box.scrollBy(10,0);return{x:window.scrollX,y:window.scrollY};})()"#,
        "the pre-change scroll payload, unchanged"
    );
    for locator in [
        "css=#email",
        r#"role=combobox[name="Size"]"#,
        "text=Sign in",
        "frame=#w >> css=#q",
    ] {
        let expr = element_expr(locator).unwrap();
        assert!(
            !expr.contains(&format!(
                "document.querySelector({})",
                serde_json::to_string(locator).unwrap()
            )),
            "{locator} must not be handed to querySelector raw: {expr}"
        );
        assert_eq!(
            Some(expr),
            loom_host::shim_manager::locator_element_js(locator),
            "{locator} resolves through the shared grammar"
        );
    }
}

/// scroll_result promotion: a valid `{x,y}` value moves into `scroll_result`
/// and `return_value_json` is cleared (single source of truth — anti-drift).
#[test]
fn promote_scroll_result_moves_value_and_clears_return_value_json() {
    let mut r = profile_restricted_evaluate_receipt(1, "sess", "p");
    r.return_value_json = Some(r#"{"x":0,"y":1400}"#.to_string());
    promote_scroll_result(&mut r);
    assert_eq!(
        r.return_value_json, None,
        "return_value_json must be cleared"
    );
    let sr = r.scroll_result.expect("scroll_result populated");
    assert_eq!(sr["y"], 1400);
    assert_eq!(sr["x"], 0);
}

/// Robustness: an unparseable value is NOT silently dropped — `return_value_json`
/// is preserved and `scroll_result` stays None. (Cannot happen for canonical
/// host JSON, but guards against silent data loss.)
#[test]
fn promote_scroll_result_preserves_unparseable_value() {
    let mut r = profile_restricted_evaluate_receipt(1, "sess", "p");
    r.return_value_json = Some("not json{".to_string());
    promote_scroll_result(&mut r);
    assert!(r.scroll_result.is_none());
    assert_eq!(r.return_value_json.as_deref(), Some("not json{"));
}

/// cdp-trusted-input: web.click is ALWAYS trusted now — intercepted host-side
/// (CDP Input.dispatchMouseEvent at the element hit point), so
/// build_chromium_args returns None (no guest Runtime.evaluate click).
#[test]
fn build_chromium_args_click_is_host_side_returns_none() {
    let action = Action::WebClick {
        session_id: s("sess"),
        selector: s("a"),
        until: None,
    };
    assert!(
        decode_cdp(&action).is_none(),
        "web.click is host-side (trusted Input.dispatchMouseEvent) → expected None"
    );
}

/// cdp-trusted-input regression: the host-side input receipt MUST carry an
/// `action_hash` (the run_e2e.sh CLI-surface test asserts every interaction
/// receipt has one) AND that hash must be SESSION-INDEPENDENT so replay stays
/// equal across sessions.
#[test]
fn build_input_dispatch_receipt_sets_session_independent_action_hash() {
    use crate::wire_receipts::build_input_dispatch_receipt;
    use loom_host::shim_manager::InputDispatchOutcome;
    let a_sess_a = Action::WebClick {
        session_id: s("sess-A"),
        selector: s("#ok-button"),
        until: None,
    };
    let a_sess_b = Action::WebClick {
        session_id: s("sess-B"),
        selector: s("#ok-button"),
        until: None,
    };
    let r_a = build_input_dispatch_receipt(1, "sess-A", &a_sess_a, InputDispatchOutcome::Ok);
    let r_b = build_input_dispatch_receipt(2, "sess-B", &a_sess_b, InputDispatchOutcome::Ok);
    assert!(
        r_a.action_hash.is_some(),
        "host-side click receipt must carry action_hash (e2e CLI-surface contract)"
    );
    assert!(
        r_a.outcome_hash.is_some(),
        "constant dispatch-marker expected"
    );
    assert_eq!(
        r_a.action_hash, r_b.action_hash,
        "action_hash must be session-independent (replay-equal across sessions)"
    );
    // Different selector → different action_hash.
    let a_other = Action::WebClick {
        session_id: s("sess-A"),
        selector: s("#other"),
        until: None,
    };
    let r_other = build_input_dispatch_receipt(3, "sess-A", &a_other, InputDispatchOutcome::Ok);
    assert_ne!(r_a.action_hash, r_other.action_hash);
}

/// The web.type action_hash is keyed on the dispatch path actually taken: a
/// bare web.type IS a fill, so it hashes like an explicit `mode:"fill"` (it
/// used to be labelled "value", the guest path it never took); keystrokes differ.
#[test]
fn web_type_action_hash_follows_the_dispatch_path() {
    use crate::wire_receipts::build_input_dispatch_receipt;
    use loom_host::shim_manager::InputDispatchOutcome;
    let typed = |mode: Option<&str>| {
        let action = Action::WebType {
            session_id: s("sess"),
            selector: s("#email"),
            text: s("user@example.com"),
            mode: mode.map(s),
            until: None,
        };
        build_input_dispatch_receipt(1, "sess", &action, InputDispatchOutcome::Ok).action_hash
    };
    assert_eq!(
        typed(None),
        typed(Some("fill")),
        "a bare web.type is a fill"
    );
    assert_ne!(typed(None), typed(Some("keystrokes")));
    assert_eq!(
        typed(None),
        Some(loom_core::content_store::sha256_hex(
            "web.type\u{0}fill\u{0}#email\u{0}user@example.com".as_bytes()
        )),
        "canonical form: verb, dispatch path, selector, text"
    );
}

/// NFR-DET-01: `DispatchedAckPending` — a click whose cross-origin navigation
/// swallowed the mouse-dispatch ack (the input WAS performed) — MUST hash
/// IDENTICALLY to `Ok`. Whether the record-time ack arrived or was lost to a
/// renderer swap must NEVER perturb the manifest hash chain, or replay would
/// diverge. The degraded readiness rides on `settle_outcome`, off the chain.
#[test]
fn dispatched_ack_pending_hashes_identically_to_ok() {
    use crate::wire_receipts::build_input_dispatch_receipt;
    use loom_host::shim_manager::InputDispatchOutcome;
    let action = Action::WebClick {
        session_id: s("sess"),
        selector: s("#go"),
        until: None,
    };
    let r_ok = build_input_dispatch_receipt(1, "sess", &action, InputDispatchOutcome::Ok);
    let r_pending = build_input_dispatch_receipt(
        1,
        "sess",
        &action,
        InputDispatchOutcome::DispatchedAckPending,
    );
    assert_eq!(
        r_ok.outcome_hash, r_pending.outcome_hash,
        "DispatchedAckPending must carry the SAME constant dispatch marker as Ok \
             (ack timing off the hash chain — replay-equal, NFR-DET-01)"
    );
    assert_eq!(
        r_ok.action_hash, r_pending.action_hash,
        "action_hash must be identical regardless of ack timing"
    );
}

/// `web.type` fill refusals are typed errors with FIXED messages: the typed
/// text (F7 — it can be a password) never appears, and nothing is hashed as a
/// dispatched outcome.
#[test]
fn fill_refusals_are_typed_errors_without_the_typed_text() {
    use crate::wire_receipts::build_input_dispatch_receipt;
    use loom_host::shim_manager::{FillFailure, InputDispatchOutcome, SetValueType};
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;
    let action = Action::WebType {
        session_id: s("sess"),
        selector: s("role=textbox[name=\"First day\"]"),
        text: s("12/31/2036-secret"),
        mode: None,
        until: None,
    };
    let cases = [
        (
            InputDispatchOutcome::MalformedValue(SetValueType::Date),
            "malformed_value",
            "yyyy-mm-dd",
        ),
        (
            InputDispatchOutcome::NotEditable,
            "not_editable",
            "disabled or readonly",
        ),
        (
            InputDispatchOutcome::FillFailed(FillFailure::Detached),
            "type_failed",
            "detached",
        ),
    ];
    for (outcome, kind, says) in cases {
        let r = build_input_dispatch_receipt(1, "sess", &action, outcome);
        assert!(matches!(r.status, ReceiptStatus::Error), "{kind}");
        let err = r.error.as_ref().expect("an error receipt");
        assert_eq!(err.kind, kind);
        let message = err.detail.as_ref().unwrap()["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.contains(says), "{kind}: {message}");
        assert!(
            !message.contains("secret"),
            "{kind} must not echo the typed text"
        );
        assert_eq!(r.outcome_hash, None, "{kind} is not a dispatched input");
        assert!(
            r.action_hash.is_some(),
            "{kind} still carries the action_hash"
        );
    }
}
