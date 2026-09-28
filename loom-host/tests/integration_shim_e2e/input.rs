//! Trusted input dispatch and `web.type` fill's prepare step.

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use std::time::Duration;

use crate::common::*;

/// e2e (cdp-trusted-input): the trusted-input senders drive the real shim →
/// fake-chromium CDP path. Trusted click resolves the box-model center and
/// dispatches `Input.dispatchMouseEvent`; keystrokes / press_key dispatch real
/// `Input.dispatchKeyEvent`; a missing selector / unknown key map to typed
/// application outcomes (not transport errors).
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn trusted_input_dispatch_round_trip() {
    use loom_host::shim_manager::InputDispatchOutcome as O;
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!("fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first");
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    // Fixture: one clickable element with a known box model.
    let fixture_path = user_data_dir.path().join("fixture.json");
    std::fs::write(
        &fixture_path,
        r##"{"boxes":{"#submit":[10.0,20.0,110.0,60.0]},"viewport":[1280,720]}"##,
    )
    .expect("write fixture");

    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-input".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_FIXTURE".into(),
                    fixture_path.display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    // Trusted click on the fixtured element → Ok (box-model center resolved +
    // mouseMoved/Pressed/Released dispatched through the real shim).
    let click = tokio::time::timeout(
        Duration::from_secs(30),
        mgr.send_trusted_click(id.clone(), 0, 0, "#submit".into(), 0),
    )
    .await
    .expect("trusted click did not return in 30s")
    .expect("trusted click transport error");
    assert_eq!(
        click,
        O::Ok,
        "trusted click on a boxed element should succeed"
    );

    // Missing selector → SelectorNotFound (typed application outcome).
    let miss = mgr
        .send_trusted_click(id.clone(), 0, 0, "#missing".into(), 0)
        .await
        .expect("trusted click transport error");
    assert_eq!(miss, O::SelectorNotFound);

    // Real per-character keystrokes into the fixtured element → Ok.
    let typed = mgr
        .send_type_keystrokes(id.clone(), 0, 0, "#submit".into(), "hi".into(), 0)
        .await
        .expect("type_keystrokes transport error");
    assert_eq!(typed, O::Ok);

    // Ambient press_key (Enter, no selector) → Ok.
    let enter = mgr
        .send_press_key(loom_host::shim_manager::SendPressKeyParams {
            id: id.clone(),
            session_id: 0,
            target_id: 0,
            key: "Enter".into(),
            selector: None,
            modifiers: vec![],
            budget_ms: 0,
        })
        .await
        .expect("press_key transport error");
    assert_eq!(enter, O::Ok);

    // Unknown key → typed UnknownKey, NOT a transport error.
    let bad = mgr
        .send_press_key(loom_host::shim_manager::SendPressKeyParams {
            id: id.clone(),
            session_id: 0,
            target_id: 0,
            key: "NoSuchKey".into(),
            selector: None,
            modifiers: vec![],
            budget_ms: 0,
        })
        .await
        .expect("press_key transport error");
    assert_eq!(bad, O::UnknownKey);

    // web.type DEFAULT (fill / Input.insertText) into the fixtured element → Ok.
    // Mirrors the keystrokes path but commits the value via a single genuine
    // `Input.insertText` (Playwright `fill()` semantics) so React/RHF onChange fires.
    let filled = mgr
        .send_type_fill(
            id.clone(),
            0,
            0,
            "#submit".into(),
            "user@example.com".into(),
            0,
        )
        .await
        .expect("type_fill transport error");
    assert_eq!(
        filled,
        O::Ok,
        "fill into a resolvable element should succeed"
    );

    // Missing selector → SelectorNotFound (typed application outcome, not a transport error).
    let fill_miss = mgr
        .send_type_fill(id.clone(), 0, 0, "#missing".into(), "x".into(), 0)
        .await
        .expect("type_fill transport error");
    assert_eq!(fill_miss, O::SelectorNotFound);

    mgr.shutdown_session("test-session-input").await;
    drop(user_data_dir);
}

/// A shim over fake-chromium whose fixture scripts web.type fill's prepare
/// verdicts (`inputs`), logging every CDP message it receives.
fn fill_prepare_manager(
    tag: &str,
    fixture: &str,
) -> (
    std::sync::Arc<ShimManager>,
    ShimId,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    assert!(
        std::path::Path::new(&fake_path).exists(),
        "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
    );
    assert!(
        std::path::Path::new(&shim_path).exists(),
        "loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first"
    );
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let fixture_path = user_data_dir.path().join("fixture.json");
    std::fs::write(&fixture_path, fixture).expect("write fixture");
    let log_path = user_data_dir.path().join("cdp_methods.log");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId(format!("chromium:{tag}"));
    let dir = user_data_dir.path().display().to_string();
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_path),
                ("LOOM_SHIM_USER_DATA_DIR".into(), dir.clone()),
                ("LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(), dir),
                (
                    "LOOM_FAKE_CHROMIUM_FIXTURE".into(),
                    fixture_path.display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_LOG".into(),
                    log_path.display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 20,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );
    (mgr, id, user_data_dir, log_path)
}

/// The CDP log written after byte offset `from`, and the log's new length.
fn cdp_log_since(log_path: &std::path::Path, from: usize) -> (String, usize) {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    (log[from.min(log.len())..].to_string(), log.len())
}

/// e2e (web-type-fills-date-inputs): fill acts on the RESOLVED node. A
/// set-value input is filled by the prepare step alone (NO `Input.insertText`);
/// a rejected value / a disabled field are typed outcomes; any other target is
/// selected, then gets one `Input.insertText`. Every prepare failure is an `Err`
/// with a fixed message (no page text) — never a fall-through to insertText.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_prepare_routes_on_the_resolved_node() {
    use loom_host::shim_manager::{FillFailure, InputDispatchOutcome as O, SetValueType};
    let sels = [
        "#text",
        "#when",
        "#bad",
        "#locked",
        "#gone",
        "#throws",
        "#odd",
        "#oddtype",
        "#noobj",
        "#unresolvable",
    ];
    let boxes = sels
        .iter()
        .map(|s| format!("\"{s}\":[10.0,20.0,110.0,60.0]"))
        .collect::<Vec<_>>()
        .join(",");
    let fixture = format!(
        r##"{{"boxes":{{{boxes}}},"inputs":{{"#when":"set","#bad":"malformed:date","#locked":"not_editable","#gone":"detached","#throws":"throw","#odd":"garbage","#oddtype":"malformed:text","#noobj":"no_object","#unresolvable":"resolve_error"}}}}"##
    );
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-prepare-routes", &fixture);
    let fill = |sel: &'static str| {
        let fut = mgr.send_type_fill(id.clone(), 0, 0, sel.into(), "2036-12-31".into(), 0);
        async move {
            tokio::time::timeout(Duration::from_secs(30), fut)
                .await
                .expect("fill did not return in 30s")
        }
    };

    // A plain editable: prepared (selected) on the resolved node, then ONE insertText.
    let (_, mut at) = cdp_log_since(&log_path, 0);
    assert_eq!(fill("#text").await.expect("fill transport error"), O::Ok);
    let (sent, next) = cdp_log_since(&log_path, at);
    at = next;
    for method in [
        "DOM.resolveNode",
        "Runtime.callFunctionOn",
        "Runtime.releaseObjectGroup",
        "Input.insertText",
    ] {
        assert!(sent.contains(method), "{method} missing; CDP log:\n{sent}");
    }
    assert!(
        !sent.contains("Runtime.evaluate"),
        "fill must not re-query the raw selector in a page evaluate; CDP log:\n{sent}"
    );

    // Set-value input: done by the prepare step — no insertText at all.
    assert_eq!(fill("#when").await.expect("fill transport error"), O::Ok);
    let (sent, next) = cdp_log_since(&log_path, at);
    at = next;
    assert!(sent.contains("Runtime.callFunctionOn"), "CDP log:\n{sent}");
    assert!(
        !sent.contains("Input.insertText"),
        "a set-value input must not also get insertText; CDP log:\n{sent}"
    );

    // Typed refusals, still without insertText.
    assert_eq!(
        fill("#bad").await.expect("fill transport error"),
        O::MalformedValue(SetValueType::Date)
    );
    assert_eq!(
        fill("#locked").await.expect("fill transport error"),
        O::NotEditable
    );
    let (sent, next) = cdp_log_since(&log_path, at);
    at = next;
    assert!(!sent.contains("Input.insertText"), "CDP log:\n{sent}");

    // A page-side reason the element cannot be filled is a TYPED outcome with a
    // fixed message (no page text), and never types.
    for (sel, failure) in [
        ("#gone", FillFailure::Detached),
        ("#throws", FillFailure::PageException),
        ("#odd", FillFailure::UnrecognisedVerdict),
        ("#oddtype", FillFailure::UnrecognisedVerdict),
        ("#noobj", FillFailure::NoObject),
        ("#unresolvable", FillFailure::Rejected),
    ] {
        let outcome = fill(sel).await.expect("fill transport error");
        assert_eq!(outcome, O::FillFailed(failure), "{sel}");
        assert!(
            !failure.message().contains("fake page exception"),
            "{sel}: page text leaked into the message"
        );
    }
    let (sent, _) = cdp_log_since(&log_path, at);
    assert!(
        !sent.contains("Input.insertText"),
        "a failed prepare must never fall through to insertText; CDP log:\n{sent}"
    );

    // Page-side failures are the page's doing, not the shim's: a run of them must
    // not trip the circuit breaker (threshold 20 here) and lock the session out.
    for _ in 0..25 {
        assert_eq!(
            fill("#gone").await.expect("fill transport error"),
            O::FillFailed(FillFailure::Detached)
        );
    }
    assert_eq!(
        fill("#text")
            .await
            .expect("the breaker must stay closed after page-side failures"),
        O::Ok
    );

    // Every fill releases only its OWN object group: no two fills share one.
    let (whole, _) = cdp_log_since(&log_path, 0);
    let groups: Vec<&str> = whole
        .lines()
        .filter(|l| l.contains("DOM.resolveNode"))
        .filter_map(|l| l.split("\"objectGroup\":\"").nth(1))
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert!(
        groups.len() >= 8,
        "expected a resolveNode per fill; got {groups:?}"
    );
    let unique: std::collections::HashSet<&&str> = groups.iter().collect();
    assert_eq!(
        unique.len(),
        groups.len(),
        "object groups reused: {groups:?}"
    );

    mgr.shutdown_session("fill-prepare-routes").await;
}

/// e2e: a lost ack in fill's prepare step is a BOUNDED error — the action's
/// budget, not the 30 s recv floor — and never a success: the text field was
/// never typed into, and a set-value input's state is unknown.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_prepare_lost_ack_is_a_bounded_error() {
    let fixture = r##"{"boxes":{"#lost-resolve":[10.0,20.0,110.0,60.0],"#lost-call":[10.0,80.0,110.0,120.0]},"inputs":{"#lost-resolve":"swallow_resolve","#lost-call":"swallow_call"}}"##;
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-lost-ack", fixture);
    for sel in ["#lost-resolve", "#lost-call"] {
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(20),
            mgr.send_type_fill(id.clone(), 0, 0, sel.into(), "2036-12-31".into(), 800),
        )
        .await
        .unwrap_or_else(|_| panic!("{sel}: a lost prepare ack must not dead-wait"))
        .expect_err(&format!(
            "{sel}: a lost prepare ack must be an error, not a success"
        ));
        let elapsed = started.elapsed();
        assert!(err.to_string().contains("unacknowledged"), "{sel}: {err}");
        assert!(
            elapsed < Duration::from_secs(10),
            "{sel}: bounded by the 800 ms budget, not the 30 s floor; took {elapsed:?}"
        );
    }
    let (sent, _) = cdp_log_since(&log_path, 0);
    assert!(
        !sent.contains("Input.insertText"),
        "a lost prepare ack must never fall through to insertText; CDP log:\n{sent}"
    );
    mgr.shutdown_session("fill-lost-ack").await;
}

/// e2e: once the action's budget is spent, fill sends nothing more that could
/// change the page — no prepare call, no insertText — and says it ran out of time.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_with_a_spent_deadline_writes_nothing() {
    // DOM.focus answers 60 ms late, so selector resolution alone spends a 10 ms
    // budget on any host (a fast CI runner resolves a warm fixture in <1 ms).
    let fixture = r##"{"boxes":{"#text":[10.0,20.0,110.0,60.0],"#when":[10.0,80.0,110.0,120.0]},"inputs":{"#when":"set"},"slow_focus_ms":60}"##;
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-spent-deadline", fixture);
    for sel in ["#text", "#when"] {
        let err = mgr
            .send_type_fill(id.clone(), 0, 0, sel.into(), "2036-12-31".into(), 10)
            .await
            .expect_err(&format!("{sel}: a spent deadline must be an error"));
        assert!(err.to_string().contains("deadline ran out"), "{sel}: {err}");
    }
    let (sent, _) = cdp_log_since(&log_path, 0);
    for write in ["Runtime.callFunctionOn", "Input.insertText"] {
        assert!(
            !sent.contains(write),
            "{write} sent after the deadline was spent; CDP log:\n{sent}"
        );
    }
    mgr.shutdown_session("fill-spent-deadline").await;
}

/// e2e: releasing a fill's CDP object group is best-effort — a failing release
/// never turns a committed fill into an error.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_release_failure_does_not_fail_a_committed_fill() {
    use loom_host::shim_manager::InputDispatchOutcome as O;
    let fixture = r##"{"boxes":{"#when":[10.0,20.0,110.0,60.0],"#text":[10.0,80.0,110.0,120.0]},"inputs":{"#when":"set"},"release_error":true}"##;
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-release-error", fixture);
    for sel in ["#when", "#text"] {
        let outcome = mgr
            .send_type_fill(id.clone(), 0, 0, sel.into(), "18:00".into(), 0)
            .await
            .unwrap_or_else(|e| panic!("{sel}: a release failure must not fail the fill: {e}"));
        assert_eq!(outcome, O::Ok, "{sel}");
    }
    let (sent, _) = cdp_log_since(&log_path, 0);
    assert!(
        sent.contains("Runtime.releaseObjectGroup"),
        "CDP log:\n{sent}"
    );
    mgr.shutdown_session("fill-release-error").await;
}
