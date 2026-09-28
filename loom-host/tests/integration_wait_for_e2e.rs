//! End-to-end proof of the standalone `web.wait_for` readiness verb
//! (settle-capture slice 2) through the REAL shim path (host →
//! `ShimManager::send_wait_for` → `loom-shim-chromium` → `fake-chromium`).
//!
//! `wait_for` reuses the exact SettleDriver / ReadinessMonitor the navigate
//! gate uses, but with NO navigation and NO capture — it waits on the current
//! page and returns only the settle verdict. These cases drive the wiring with
//! `LOOM_FAKE_CHROMIUM_SCRIPT` (see `fake-chromium/settle_script.rs`) and assert the verdict
//! reaches `WaitOutcome` end-to-end:
//!
//!   - clean page → `reached` (the default fake page settles immediately).
//!   - never-settles (network): perpetual in-flight > idle threshold → typed
//!     `timeout`, `network_count_at_settle >= 3`, no hang.
//!   - never-settles (DOM): perpetual mutations, all else quiet → typed
//!     `dom_unstable`, distinct from the network timeout.
//!
//! Run:
//!   cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium
//!   cargo build -p loom-cli --bin loom-shim-chromium
//!   cargo test -p loom-host --test integration_wait_for_e2e -- --ignored

#![cfg(unix)]

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use std::time::Duration;

fn target_bin_dir() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    let deps = test_exe.parent().expect("deps dir");
    deps.parent().expect("debug dir").to_path_buf()
}

fn shim_bin() -> String {
    target_bin_dir()
        .join("loom-shim-chromium")
        .to_string_lossy()
        .into_owned()
}

fn fake_chromium_bin() -> String {
    target_bin_dir()
        .join("fake-chromium")
        .to_string_lossy()
        .into_owned()
}

fn assert_binaries_built() {
    for (path, build) in [
        (
            fake_chromium_bin(),
            "cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium",
        ),
        (
            shim_bin(),
            "cargo build -p loom-cli --bin loom-shim-chromium",
        ),
    ] {
        if !std::path::Path::new(&path).exists() {
            panic!("missing binary {path}; run `{build}` first");
        }
    }
}

/// Build a ShimManager whose `fake-chromium` is driven by `script_json`.
fn make_manager_with_script(
    session_label: &str,
    script_json: &str,
) -> (std::sync::Arc<ShimManager>, ShimId, tempfile::TempDir) {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let script_path = user_data_dir.path().join("settle_script.json");
    std::fs::write(&script_path, script_json).expect("write settle script");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId(format!("chromium:{session_label}"));
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_bin().into(),
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
                    "LOOM_FAKE_CHROMIUM_SCRIPT".into(),
                    script_path.display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 30_000,
            recv_timeout_ms: 60_000,
        },
    );
    (mgr, id, user_data_dir)
}

async fn wait_for_settled(
    mgr: &std::sync::Arc<ShimManager>,
    id: &ShimId,
) -> loom_shared::navigate_outcome::WaitOutcome {
    // 180s outer guard: the never-settles cases walk the full 2000-tick
    // ceiling, whose wall-clock (ceiling × per-tick CDP round-trip) can
    // approach ~30s on a slow CI runner. The guard only exists to fail loudly
    // on a TRUE infinite hang, so it needs ample headroom. The clean case
    // settles in ~25ms and never approaches it.
    tokio::time::timeout(
        Duration::from_secs(180),
        mgr.send_wait_for(loom_host::shim_manager::SendWaitForParams {
            id: id.clone(),
            action_id: "test-action".to_string(),
            session_id: 0,
            target_id: 0,
            until: "settled".to_string(),
            budget_ms: 30_000,
            seed: loom_shared::types::Seed(0),
            epoch_ms: loom_shared::types::EpochMs(0),
            determinism_enabled: true,
            audio_enabled: false,
        }),
    )
    .await
    .expect("send_wait_for timed out (a never-settles case must return a TYPED verdict, not hang)")
    .expect("send_wait_for returned an error")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_clean_page_reaches_settled() {
    assert_binaries_built();
    // Default script → page is immediately complete, stable, quiet.
    let script = r#"{ "settle_probe": [[true, "http://fake.test/app", 0]] }"#;
    let (mgr, id, _udd) = make_manager_with_script("waitfor-reached", script);

    let outcome = wait_for_settled(&mgr, &id).await;

    assert_eq!(outcome.settle_until, "settled");
    assert_eq!(
        outcome.settle_outcome, "reached",
        "a clean current page must settle to reached"
    );
    // wait_for runs the quiet window (>= 5 ticks) before declaring settled.
    assert!(
        outcome.settle_ms >= 25,
        "got settle_ms={}",
        outcome.settle_ms
    );

    mgr.shutdown_session("waitfor-reached").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_never_settles_network_times_out() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[true, "http://fake.test/poll", 0]],
        "perpetual_inflight": 3
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("waitfor-net-timeout", script);

    let outcome = wait_for_settled(&mgr, &id).await;

    assert_eq!(
        outcome.settle_outcome, "timeout",
        "persistent in-flight requests must hit the bounded network timeout"
    );
    assert!(
        outcome.network_count_at_settle >= 3,
        "got network_count_at_settle={}",
        outcome.network_count_at_settle
    );

    mgr.shutdown_session("waitfor-net-timeout").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_never_settles_dom_is_dom_unstable() {
    assert_binaries_built();
    let script = r#"{ "settle_probe": [[true, "http://fake.test/anim", 4]] }"#;
    let (mgr, id, _udd) = make_manager_with_script("waitfor-dom-unstable", script);

    let outcome = wait_for_settled(&mgr, &id).await;

    assert_eq!(
        outcome.settle_outcome, "dom_unstable",
        "a perpetually-mutating DOM (all else quiet) must report dom_unstable"
    );

    mgr.shutdown_session("waitfor-dom-unstable").await;
}

/// Variant of `make_manager_with_script` that ALSO captures the fake-chromium CDP
/// method log (`LOOM_FAKE_CHROMIUM_LOG`) inside the tempdir, returning its path so
/// a test can assert the exact CDP command sequence the shim issued.
fn make_manager_with_script_and_log(
    session_label: &str,
    script_json: &str,
) -> (
    std::sync::Arc<ShimManager>,
    ShimId,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let script_path = user_data_dir.path().join("settle_script.json");
    std::fs::write(&script_path, script_json).expect("write settle script");
    let log_path = user_data_dir.path().join("cdp_methods.log");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId(format!("chromium:{session_label}"));
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_bin().into(),
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
                    "LOOM_FAKE_CHROMIUM_SCRIPT".into(),
                    script_path.display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_LOG".into(),
                    log_path.display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 30_000,
            recv_timeout_ms: 60_000,
        },
    );
    (mgr, id, user_data_dir, log_path)
}

/// Regression guard for `auth0-ulp-submit`: a standalone `web.wait_for` MUST arm a
/// bounded virtual-time BUDGET so the page's PENDING timers advance under the
/// determinism clock pin.
///
/// Before the fix, `wait_for` only re-armed a budget AFTER a navigation had
/// already begun (`renavigated`); on the common path it settled the current page
/// with no budget arm at all. Under the deterministic virtual clock that is frozen
/// after the prior navigate's budget drained, a preceding interaction verb
/// (`web.click` / `web.press_key`) that ran a handler scheduling async work behind
/// a `setTimeout` (e.g. Auth0 New ULP's react-hook-form `onSubmit`: async validate
/// → `navigator.credentials` probe → `fetch(POST)`) stalled at that first
/// macrotask — so the page never began the navigation `wait_for` was meant to
/// observe, and `wait_for` returned `reached` on the still-`complete` document.
///
/// Asserting the shim issued a budget-carrying `Emulation.setVirtualTimePolicy`
/// during a BARE wait_for (no navigate) pins the fix through the real host → shim
/// path. The budgetless `policy:"pause"` inject pin does NOT count — it carries no
/// `budget` and is present with or without the fix.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_arms_virtual_time_budget_for_pending_timers() {
    assert_binaries_built();
    // Default clean page: complete, stable, quiet — no navigation in flight. This
    // is exactly the shape that exposed the bug (the old common path settled
    // immediately WITHOUT arming a budget).
    let script = r#"{ "settle_probe": [[true, "http://fake.test/app", 0]] }"#;
    let (mgr, id, _udd, log_path) = make_manager_with_script_and_log("waitfor-vt-budget", script);

    let outcome = wait_for_settled(&mgr, &id).await;
    assert_eq!(
        outcome.settle_outcome, "reached",
        "the page must still settle cleanly with the budget armed"
    );

    mgr.shutdown_session("waitfor-vt-budget").await;

    // At least one budget-carrying `setVirtualTimePolicy` must appear in the CDP
    // log for this wait_for. String match is sufficient + dependency-free: the
    // budget-arm line contains both the method and a `"budget"` key; the inject
    // pin line contains the method but no `"budget"`.
    let log = std::fs::read_to_string(&log_path).expect("read cdp log");
    let armed_budget = log
        .lines()
        .any(|l| l.contains("setVirtualTimePolicy") && l.contains("\"budget\""));
    assert!(
        armed_budget,
        "web.wait_for must arm a budget-carrying setVirtualTimePolicy so pending \
         timers advance (auth0-ulp-submit). CDP log was:\n{log}"
    );
}

/// Companion regression guard for the staging Auth0 wedge: a `--no-determinism`
/// session (real wall-clock, clock never pinned at inject) must NOT arm a
/// virtual-time budget on wait_for. Pre-fix, the arm was gated on the process-
/// global capture flag (`virtual_time_enabled()`), so `--no-determinism` sessions
/// armed a budget and then hung awaiting a `virtualTimeBudgetExpired` that never
/// fires on a real clock (the 2nd authed navigate to a heavy cross-origin SPA
/// wedged the whole session). The fix gates the arm on whether the clock was
/// ACTUALLY pinned (`determinism_injector::clock_pinned`, set only when inject
/// ran under `determinism_enabled`). This pins that through the real host → shim
/// path: with `determinism_enabled: false` the shim must issue NO budget-carrying
/// `setVirtualTimePolicy`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn no_determinism_wait_for_does_not_arm_virtual_time_budget() {
    assert_binaries_built();
    // Clean page — the same shape the determinism-ON budget test uses.
    let script = r#"{ "settle_probe": [[true, "http://fake.test/app", 0]] }"#;
    let (mgr, id, _udd, log_path) = make_manager_with_script_and_log("waitfor-no-det-vt", script);

    let outcome = tokio::time::timeout(
        Duration::from_secs(180),
        mgr.send_wait_for(loom_host::shim_manager::SendWaitForParams {
            id: id.clone(),
            action_id: "test-action".to_string(),
            session_id: 0,
            target_id: 0,
            until: "settled".to_string(),
            budget_ms: 30_000,
            seed: loom_shared::types::Seed(0),
            epoch_ms: loom_shared::types::EpochMs(0),
            // The session runs on the REAL wall-clock; the clock is never pinned.
            determinism_enabled: false,
            audio_enabled: false,
        }),
    )
    .await
    .expect(
        "send_wait_for timed out (no_determinism wait_for must return a typed verdict, not hang)",
    )
    .expect("send_wait_for returned an error");

    assert_eq!(
        outcome.settle_outcome, "reached",
        "no_determinism wait_for must still settle the clean page on the real clock"
    );

    mgr.shutdown_session("waitfor-no-det-vt").await;

    // NO budget-carrying setVirtualTimePolicy may appear: the session never pinned
    // the clock, so arming a budget — and then awaiting a virtualTimeBudgetExpired
    // that can't fire on a real clock — must not happen.
    let log = std::fs::read_to_string(&log_path).expect("read cdp log");
    let armed_budget = log
        .lines()
        .any(|l| l.contains("setVirtualTimePolicy") && l.contains("\"budget\""));
    assert!(
        !armed_budget,
        "no_determinism wait_for must NOT arm a budget-carrying setVirtualTimePolicy \
         (it would await a virtualTimeBudgetExpired that never fires on the real clock — \
         the staging Auth0 wedge). CDP log was:\n{log}"
    );
}
