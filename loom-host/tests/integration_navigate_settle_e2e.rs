//! End-to-end proof that the settle-capture readiness gate delivers the
//! brief's reproduction shapes through the REAL shim path (host →
//! `loom-shim-chromium` → `fake-chromium`), not just at the pure
//! state-machine layer.
//!
//! The 16 `ReadinessMachine` unit tests (`loom-shims/src/readiness_monitor/
//! interface_tests.rs`) already prove the verdict logic for every branch from
//! a scripted observation feed. THESE tests prove the WIRING: that a scripted
//! page actually drives the live `SettleDriver` → `ReadinessMachine` →
//! `NavigateOutcome` chain to the same verdict. Each case scripts the page via
//! `LOOM_FAKE_CHROMIUM_SCRIPT` (a per-tick settle-probe feed + optional
//! perpetual in-flight requests; see `fake-chromium/settle_script.rs`):
//!
//!   (a) Client-side redirect SPA: the probe reports the shell URL, then the
//!       final URL, then quiesces. Assert the capture is gated until AFTER the
//!       redirect (`reached`, having run the full quiet window) — the old
//!       commit-time capture would have grabbed the blank shell.
//!   (c) Async-after-load: `readyState==complete` from tick 1, but a delayed
//!       DOM-mutation burst arrives later. Assert `settled` waits for it
//!       (`reached`, strictly longer than a clean page's minimum settle) — a
//!       naive readyState-only wait would have captured too early.
//!   (d1) Never-settles (network): perpetual in-flight requests above the idle
//!       threshold. Assert the bounded tick-ceiling returns a typed `timeout`
//!       receipt and does NOT hang.
//!   (d2) Never-settles (DOM): the DOM mutates every tick forever (network
//!       quiet, document complete). Assert a typed `dom_unstable` receipt,
//!       distinct from the network timeout.
//!   (e) Static page under determinism: the inject-time virtual-time pin
//!       defers the load event until the budget arm (mirroring real headless
//!       Chromium). Assert `reached` well inside the budget — the
//!       settle-timeout-on-static regression pin.
//!
//! No wall-clock dependence: every verdict is a pure function of the scripted
//! per-tick observation sequence (DET-CORE). `settle_ms` is `ticks * pacing`,
//! a deterministic integer, so the `>=` bounds below are stable.
//!
//! Run:
//!   cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium
//!   cargo build -p loom-cli --bin loom-shim-chromium
//!   cargo test -p loom-host --test integration_navigate_settle_e2e -- --ignored
//!
//! Marked `#[ignore]` so a default `cargo test --workspace` doesn't force the
//! fake-chromium build.

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
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!(
            "loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first"
        );
    }
}

/// Build a ShimManager whose `fake-chromium` is driven by `script_json`
/// (written into the per-session user-data dir and pointed at via
/// `LOOM_FAKE_CHROMIUM_SCRIPT`). The returned `TempDir` owns both the
/// user-data dir and the script file; keep it alive for the test's duration.
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

/// Drive one `settled` navigate against the scripted fake page.
async fn navigate_settled(
    mgr: &std::sync::Arc<ShimManager>,
    id: &ShimId,
    outer_timeout: Duration,
) -> loom_shared::navigate_outcome::NavigateOutcome {
    tokio::time::timeout(
        outer_timeout,
        mgr.send_navigate(loom_host::shim_manager::SendNavigateParams {
            id: id.clone(),
            action_id: "test-action".to_string(),
            session_id: 0,
            target_id: 0,
            url: "http://fake.test/status/200".into(),
            budget_ms: 30_000,
            seed: loom_shared::types::Seed(0),
            epoch_ms: loom_shared::types::EpochMs(0),
            blocklist_enabled: true,
            until: "settled".to_string(),
            determinism_enabled: true,
            audio_enabled: false,
        }),
    )
    .await
    .expect("send_navigate timed out (a never-settles case must return a TYPED receipt, not hang)")
    .expect("send_navigate returned an error")
}

/// Drive one `settled` navigate with an explicit `determinism_enabled` flag.
/// (The `navigate_settled` wrapper above is the determinism-ON case; the
/// navigate-degradation regression needs the `--no-determinism` path.)
async fn navigate_with_determinism(
    mgr: &std::sync::Arc<ShimManager>,
    id: &ShimId,
    outer_timeout: Duration,
    determinism_enabled: bool,
) -> loom_shared::navigate_outcome::NavigateOutcome {
    tokio::time::timeout(
        outer_timeout,
        mgr.send_navigate(loom_host::shim_manager::SendNavigateParams {
            id: id.clone(),
            action_id: "test-action".to_string(),
            session_id: 0,
            target_id: 0,
            url: "http://fake.test/status/200".into(),
            budget_ms: 30_000,
            seed: loom_shared::types::Seed(0),
            epoch_ms: loom_shared::types::EpochMs(0),
            blocklist_enabled: true,
            until: "settled".to_string(),
            determinism_enabled,
            audio_enabled: false,
        }),
    )
    .await
    .expect("send_navigate timed out (the degraded session must not hang the whole test)")
    .expect("send_navigate returned an error")
}

/// Drive one `settled` wait_for against the scripted fake page (idempotent
/// spawn — no prior navigate needed).
async fn wait_for_settled(
    mgr: &std::sync::Arc<ShimManager>,
    id: &ShimId,
    outer_timeout: Duration,
) -> loom_shared::navigate_outcome::WaitOutcome {
    tokio::time::timeout(
        outer_timeout,
        mgr.send_wait_for(loom_host::shim_manager::SendWaitForParams {
            id: id.clone(),
            action_id: "test-wait".to_string(),
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
    .expect("send_wait_for timed out (a never-settles case must return a TYPED receipt, not hang)")
    .expect("send_wait_for returned an error")
}

// ── (client-nav-reattach) navigate re-attaches after a client-side top-level ──
// navigation (window.location / <meta refresh> / form-POST). The loaded shell
// completes, then begins a self-initiated navigation whose new document is held
// `readyState:"loading"` until loom RE-ARMS the virtual-time budget. Today loom
// arms the budget once-per-navigate and never re-arms, so it wedges on the blank
// in-flight document and the settle gate reports `timeout`. After the fix it must
// detect the renavigation, re-arm, and settle on the FINAL document (`reached`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn navigate_reattaches_after_client_redirect() {
    assert_binaries_built();
    // idx0: shell loading. idx1: shell complete (a wedged loom would capture
    // THIS blank shell). idx2: the page client-redirects to the IdP — a NEW
    // top-level document whose load is GATED on a second budget arm
    // (`renavigate_at:[2]`); it stays "loading" until loom re-arms. idx3+: IdP
    // complete + quiet → settles, but ONLY if loom re-attached.
    let script = r#"{
        "settle_probe": [
            [false, "http://fake.test/shell", 0],
            [true,  "http://fake.test/shell", 2],
            [false, "http://fake.test/idp",   0],
            [true,  "http://fake.test/idp",   0],
            [true,  "http://fake.test/idp",   0]
        ],
        "renavigate_at": [2]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("reattach-nav", script);

    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(30)).await;

    // RED today: loom never re-arms the second document's budget, so the probe
    // stays "loading" and the gate returns `timeout` on the blank IdP shell.
    // GREEN after the fix: loom re-attaches + re-settles on the final page.
    assert_eq!(
        outcome.settle_outcome, "reached",
        "loom must re-attach to the client-redirected document and settle on it, \
         not wedge on the blank in-flight shell (got settle_outcome={}, settle_ms={})",
        outcome.settle_outcome, outcome.settle_ms
    );
    assert_eq!(outcome.settle_until, "settled");

    mgr.shutdown_session("reattach-nav").await;
}

// ── (client-nav-reattach) wait_for resolves on the renavigated document ───────
// Acceptance #2: after a form-POST submit leaves the page on a blank in-flight
// document, a `web.wait_for` must re-attach + resolve on the new (password-step)
// document — NOT hang on the detached one. wait_for never arms virtual time
// today, so only a fresh navigate recovers; the fix gives wait_for the same
// bounded re-arm path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_resolves_on_renavigated_document() {
    assert_binaries_built();
    // idx0: the page is mid-navigation (post form-POST), held "loading" until a
    // budget re-arm (`renavigate_at:[0]`). idx1+: password step complete + quiet.
    let script = r#"{
        "settle_probe": [
            [false, "http://fake.test/password", 0],
            [true,  "http://fake.test/password", 0],
            [true,  "http://fake.test/password", 0]
        ],
        "renavigate_at": [0]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("reattach-wait", script);

    let outcome = wait_for_settled(&mgr, &id, Duration::from_secs(30)).await;

    assert_eq!(
        outcome.settle_outcome, "reached",
        "wait_for must re-attach + re-arm so it resolves on the new document, \
         not hang on the wedged in-flight one (got settle_outcome={}, settle_ms={})",
        outcome.settle_outcome, outcome.settle_ms
    );
    assert_eq!(outcome.settle_until, "settled");

    mgr.shutdown_session("reattach-wait").await;
}

// ── (client-nav-reattach) replay-equality across the re-attach path ───────────
// FND-0019: re-attaching changes WHICH document is captured (final, not blank
// shell), but the whole shell→redirect→final sequence runs under the paused
// virtual clock, so two same-seed records must produce byte-identical settle
// outcomes + DOM hash. `renavigated` is shim-internal and never enters the hash.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn reattach_redirect_replays_equal() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [
            [false, "http://fake.test/shell", 0],
            [true,  "http://fake.test/shell", 2],
            [false, "http://fake.test/idp",   0],
            [true,  "http://fake.test/idp",   0],
            [true,  "http://fake.test/idp",   0]
        ],
        "renavigate_at": [2]
    }"#;

    let (mgr1, id1, _udd1) = make_manager_with_script("reattach-det-1", script);
    let a = navigate_settled(&mgr1, &id1, Duration::from_secs(30)).await;
    mgr1.shutdown_session("reattach-det-1").await;

    let (mgr2, id2, _udd2) = make_manager_with_script("reattach-det-2", script);
    let b = navigate_settled(&mgr2, &id2, Duration::from_secs(30)).await;
    mgr2.shutdown_session("reattach-det-2").await;

    assert_eq!(a.settle_outcome, "reached");
    assert_eq!(
        a.settle_outcome, b.settle_outcome,
        "settle_outcome must replay-equal"
    );
    assert_eq!(
        a.settle_until, b.settle_until,
        "settle_until must replay-equal"
    );
    assert_eq!(
        a.dom_after_sha256, b.dom_after_sha256,
        "same-seed re-attach must capture a byte-identical final DOM (NFR-DET-01)"
    );
}

// ── (client-nav-reattach) redirect chain is bounded by MAX_REATTACH_HOPS ───────
// FND-0016: a page that bounces between login states forever must NOT hang — the
// re-attach loop caps the followed chain (10) and returns the typed `timeout`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn reattach_redirect_chain_is_bounded() {
    assert_binaries_built();
    // 12 redirect hops (> the cap of 10): each hop completes then immediately
    // renavigates. probe idx 2,4,..,24 are the renavigation points; the odd
    // indices between them report the just-loaded (complete) interstitial.
    let mut probe = String::from(
        "[false, \"http://fake.test/shell\", 0],\n            [true, \"http://fake.test/shell\", 2]",
    );
    let mut renav = Vec::new();
    for k in 1..=12 {
        let renav_idx = 2 * k; // even indices: a new redirect begins (loading)
        renav.push(renav_idx.to_string());
        probe.push_str(&format!(
            ",\n            [false, \"http://fake.test/loop{k}\", 0]"
        ));
        probe.push_str(&format!(
            ",\n            [true, \"http://fake.test/loop{k}\", 0]"
        ));
    }
    let script = format!(
        "{{\n            \"settle_probe\": [\n            {probe}\n            ],\n            \"renavigate_at\": [{}]\n        }}",
        renav.join(", ")
    );
    let (mgr, id, _udd) = make_manager_with_script("reattach-bounded", &script);

    // Outer timeout well above the per-action budget: the loop must self-bound on
    // the hop cap, NOT by hanging until this outer guard.
    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(60)).await;

    assert_eq!(
        outcome.settle_outcome, "timeout",
        "an unbounded redirect loop must return the typed timeout (bounded by \
         MAX_REATTACH_HOPS), got settle_outcome={}",
        outcome.settle_outcome
    );

    mgr.shutdown_session("reattach-bounded").await;
}

// ── (a) Client-side redirect SPA: settle waits past the blank shell ──────────
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn redirect_spa_settles_on_final_page_not_shell() {
    assert_binaries_built();
    // tick1: shell, not yet complete. tick2: shell complete (a commit-time
    // capture would grab THIS — the blank shell). tick3: client-side redirect
    // to the final URL. tick4+: final URL, quiet → eventually settles.
    let script = r#"{
        "settle_probe": [
            [false, "http://fake.test/shell", 0],
            [true,  "http://fake.test/shell", 2],
            [true,  "http://fake.test/final", 0],
            [true,  "http://fake.test/final", 0]
        ]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("settle-redirect", script);

    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(30)).await;

    assert_eq!(
        outcome.settle_outcome, "reached",
        "the redirect quiesces, so settled must be reached"
    );
    assert_eq!(outcome.settle_until, "settled");
    // The gate ran the full quiet window AFTER the shell→final redirect; a
    // commit-time capture (settle_ms == 0) would have grabbed the blank shell.
    assert!(
        outcome.settle_ms >= 25,
        "settled must wait through the redirect + quiet window, got settle_ms={}",
        outcome.settle_ms
    );

    mgr.shutdown_session("settle-redirect").await;
}

// ── (c) Async content injected AFTER readyState=complete is awaited ──────────
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn async_content_after_load_is_awaited() {
    assert_binaries_built();
    // readyState==complete from tick 1 (a naive readyState-only wait stops
    // here), but a delayed fetch injects DOM mutations at ticks 3–4 before the
    // page finally quiesces. `settled` must wait for the late content.
    let script = r#"{
        "settle_probe": [
            [true, "http://fake.test/app", 0],
            [true, "http://fake.test/app", 0],
            [true, "http://fake.test/app", 5],
            [true, "http://fake.test/app", 5],
            [true, "http://fake.test/app", 0]
        ]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("settle-async", script);

    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(30)).await;

    assert_eq!(
        outcome.settle_outcome, "reached",
        "the async content eventually quiesces, so settled must be reached"
    );
    // A clean page settles in exactly the quiet window (settle_ms == 25). The
    // late mutation burst resets the DOM-quiet run, so this MUST take strictly
    // longer — proof the gate awaited content a readyState-only wait would miss.
    assert!(
        outcome.settle_ms > 25,
        "settled must wait past the async DOM burst (strictly longer than a clean \
         page's minimum), got settle_ms={}",
        outcome.settle_ms
    );

    mgr.shutdown_session("settle-async").await;
}

// ── (d1) Never-settles (network): bounded timeout, typed receipt, no hang ────
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn never_settles_network_returns_bounded_timeout() {
    assert_binaries_built();
    // The page is complete and the DOM is quiet, but 3 requests (> the idle
    // threshold of 2) stay in flight forever — a persistent connection /
    // perpetual poll. `networkidle` can never be met → the tick ceiling must
    // return a typed `timeout` (NOT `dom_unstable`, NOT a hang).
    let script = r#"{
        "settle_probe": [[true, "http://fake.test/poll", 0]],
        "perpetual_inflight": 3
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("settle-net-timeout", script);

    // Bounded by the shim's DEFAULT_NAVIGATE_BUDGET (10s) → 2000-tick ceiling.
    // The VERDICT is deterministic (tick-count based), but the wall-clock to
    // walk the ceiling = ceiling × (tick pacing + per-tick CDP round-trip),
    // which on a slow/loaded CI runner can approach ~30s — so this outer guard
    // (which only exists to fail loudly on a TRUE infinite hang) needs ample
    // headroom over that. 180s is ~5× the observed CI wall-time.
    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(180)).await;

    assert_eq!(
        outcome.settle_outcome, "timeout",
        "persistent in-flight requests must hit the bounded network timeout"
    );
    assert!(
        outcome.network_count_at_settle >= 3,
        "the timeout receipt must report the stuck in-flight count, got {}",
        outcome.network_count_at_settle
    );

    mgr.shutdown_session("settle-net-timeout").await;
}

// ── (d2) Never-settles (DOM): typed dom_unstable, distinct from timeout ──────
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn never_settles_dom_returns_dom_unstable() {
    assert_binaries_built();
    // Network quiet, document complete, URL stable — but the DOM mutates every
    // single tick forever (a perpetual animation / re-render). Everything
    // EXCEPT the DOM quiesces, so the ceiling must return `dom_unstable`,
    // distinct from the network `timeout` above.
    let script = r#"{
        "settle_probe": [[true, "http://fake.test/anim", 4]]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("settle-dom-unstable", script);

    // 180s outer guard: same 2000-tick ceiling wall-time rationale as the
    // never-settles-network case (the verdict is deterministic; only the
    // wall-clock to reach the ceiling varies with CI runner speed).
    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(180)).await;

    assert_eq!(
        outcome.settle_outcome, "dom_unstable",
        "a perpetually-mutating DOM (everything else quiet) must report dom_unstable"
    );

    mgr.shutdown_session("settle-dom-unstable").await;
}

// ── (e) Static page under determinism: settled is REACHED, well in budget ────
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn static_page_under_determinism_reaches_settled() {
    assert_binaries_built();
    // A trivially static page: ready immediately, URL stable, zero mutations.
    // Under determinism the inject-time virtual-time clock pin DEFERS
    // `Page.loadEventFired` until the navigate arms the per-navigation budget
    // (fake-chromium mirrors real headless Chromium here). The v0.10.1
    // executor awaited load BEFORE arming the budget, so `load_fired` stayed
    // false, the settled latch (which requires it) could never close, and
    // every navigate burned its full wall-clock budget before reporting
    // `settle_outcome="timeout"` on a fully-loaded page
    // (settle-timeout-on-static). This pins the fix.
    let script = r#"{
        "settle_probe": [[true, "http://fake.test/static", 0]]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script("settle-static", script);

    let started = std::time::Instant::now();
    let outcome = navigate_settled(&mgr, &id, Duration::from_secs(30)).await;
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.settle_outcome, "reached",
        "a loaded, request-quiet, mutation-quiet page must reach settled"
    );
    assert_eq!(outcome.settle_until, "settled");
    // A clean page settles in exactly the quiet window (5 ticks → 25ms of
    // virtual settle time); generous headroom, but far below the ceiling.
    assert!(
        outcome.settle_ms <= 100,
        "a clean page must settle in ~the quiet window, got settle_ms={}",
        outcome.settle_ms
    );
    // Well inside the 10s default navigate budget: the OLD order spent the
    // full budget waiting for a load event that cannot fire while the clock
    // pin holds, then walked the settle ceiling on top.
    assert!(
        elapsed < Duration::from_secs(8),
        "static-page navigate must complete well inside the budget, took {elapsed:?}"
    );

    mgr.shutdown_session("settle-static").await;
}

// ── (navigate-degradation) repeated navigate in ONE session does NOT degrade ──
// Regression for the "+20s per web.navigate, then the session wedges" bug. Under
// `--no-determinism` (`determinism_enabled=false`) loom still arms + drains a
// per-navigation virtual-time budget (animation-capture runs regardless of
// determinism). Real headless Chromium — and now fake-chromium — leaves virtual
// time PAUSED once a budget drains. The unfixed exit guard (`!budget_drained`)
// skipped the renderer RESUME after a clean drain, so the clock stayed paused and
// the NEXT navigate's `Page.loadEventFired` was deferred; the determinism-OFF
// navigate path awaits load BEFORE re-arming, so every navigate after the first
// burned its full ~10s load-wait budget, compounding until the 30s host deadline
// wedged the session. The resume-guard fix (`should_resume_virtual_clock`) un-pauses
// the clock on navigate exit whenever determinism is OFF, so every navigate starts
// on an advancing clock and settles promptly. The session (hence the paused-clock
// state) persists across the loop — exactly the multi-navigate journey that broke.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn repeated_navigate_no_determinism_does_not_degrade() {
    assert_binaries_built();
    let script = r#"{ "settle_probe": [[true, "http://fake.test/static", 0]] }"#;
    let (mgr, id, _udd) = make_manager_with_script("nav-degradation", script);

    // Drive several navigates IN ONE SESSION under --no-determinism. The first is
    // always fast; navigates 2..N are where the clock-left-paused regression bit
    // (each burned the full load-wait budget on an unfixed build).
    for i in 1..=4 {
        let started = std::time::Instant::now();
        let outcome = navigate_with_determinism(&mgr, &id, Duration::from_secs(30), false).await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome.settle_outcome, "reached",
            "navigate #{i} must settle (got settle_outcome={}, settle_ms={})",
            outcome.settle_outcome, outcome.settle_ms
        );
        // THE REGRESSION ASSERTION: a degraded navigate spent its full ~10s
        // load-wait budget waiting for a load event deferred under the paused
        // clock. The fix keeps every navigate well inside the budget (sub-second).
        // 8s cleanly separates fixed (<1s) from the ~10s-per-call degradation —
        // the same wall-clock bound the determinism static-page test uses.
        assert!(
            elapsed < Duration::from_secs(8),
            "navigate #{i} under --no-determinism must complete promptly with no \
             per-call budget burn; took {elapsed:?} — the virtual clock was left \
             paused and the load event deferred (navigate-degradation)",
        );
    }

    mgr.shutdown_session("nav-degradation").await;
}

// ── (click-cross-origin-until) cross-origin process-swap settle is BOUNDED ────
//
// Regression for the v0.15.0 bug: a `web.click`/`web.wait_for` whose navigation
// crosses origin (app → checkout.stripe.com) makes the renderer PROCESS SWAP, so
// the virtual-time budget armed on the stale CDP session never expires and the
// destination's load event never reaches it. Pre-fix, the shim's standalone
// `wait_for` runs THREE sequential phases (VT-arm+await, settle_with_reattach,
// resume) that EACH re-take the full per-phase budget, so a completed cross-origin
// navigation burns ~2–3× the budget — exceeding the daemon settle budget and the
// RPC per-call deadline, which the caller sees as a transport `request_timeout`
// ("the click failed", which is FALSE — the nav succeeded).
//
// The fix threads the daemon budget to the shim and puts ALL phases under ONE
// shared wall-clock deadline, so the wait is bounded by ~1× budget and always
// returns a TYPED `settle_outcome` (never a transport error / hang).
//
// This helper shrinks the per-phase base budget via `LOOM_SHIM_CDP_TIMEOUT_MS`
// so the pre-fix multi-phase overrun (~2–3×) is cleanly separated from the
// post-fix single-budget bound by the wall-clock assertion below.
fn make_manager_with_script_env(
    session_label: &str,
    script_json: &str,
    extra_env: Vec<(String, String)>,
) -> (std::sync::Arc<ShimManager>, ShimId, tempfile::TempDir) {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let script_path = user_data_dir.path().join("settle_script.json");
    std::fs::write(&script_path, script_json).expect("write settle script");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId(format!("chromium:{session_label}"));
    let mut env = vec![
        ("LOOM_SHIM_CHROMIUM_PATH".to_string(), fake_chromium_bin()),
        (
            "LOOM_SHIM_USER_DATA_DIR".to_string(),
            user_data_dir.path().display().to_string(),
        ),
        (
            "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".to_string(),
            user_data_dir.path().display().to_string(),
        ),
        (
            "LOOM_FAKE_CHROMIUM_SCRIPT".to_string(),
            script_path.display().to_string(),
        ),
    ];
    env.extend(extra_env);
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_bin().into(),
            args: vec![],
            env,
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 30_000,
            recv_timeout_ms: 60_000,
        },
    );
    (mgr, id, user_data_dir)
}

/// Drive one `wait_for` with an explicit `until` + `budget_ms`, measuring the
/// wall-clock the call actually consumed (the load-bearing signal for the
/// multi-phase-overrun regression).
async fn wait_for_timed(
    mgr: &std::sync::Arc<ShimManager>,
    id: &ShimId,
    until: &str,
    budget_ms: u64,
    outer_timeout: Duration,
) -> (loom_shared::navigate_outcome::WaitOutcome, Duration) {
    let start = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        outer_timeout,
        mgr.send_wait_for(loom_host::shim_manager::SendWaitForParams {
            id: id.clone(),
            action_id: "test-xorigin".to_string(),
            session_id: 0,
            target_id: 0,
            until: until.to_string(),
            budget_ms,
            seed: loom_shared::types::Seed(0),
            epoch_ms: loom_shared::types::EpochMs(0),
            determinism_enabled: true,
            audio_enabled: false,
        }),
    )
    .await
    .expect(
        "send_wait_for exceeded the OUTER guard — a cross-origin swap must return a \
         TYPED verdict inside its budget, never hang (this is the never-hangs guarantee)",
    )
    .expect(
        "send_wait_for returned a transport ERROR for a cross-origin navigation — the \
         caller would read this as 'the click failed', which is FALSE; it must return a \
         success WaitOutcome with a typed settle_outcome",
    );
    (outcome, start.elapsed())
}

// (a) THE REGRESSION: a cross-origin process swap with `until:"load"` must return
// a bounded, typed verdict inside ~1× budget — NOT burn 2–3× the budget nor
// surface a transport error. `cross_origin_swap_at:[0]` makes the fake suppress
// the destination's load event + budget expiry on re-arm (they land on a renderer
// the stale session cannot see) and error the settle probe (destroyed context).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_cross_origin_swap_is_bounded_and_typed() {
    assert_binaries_built();
    // idx0: the click's navigation swaps cross-origin; the stale session goes
    // blind. The trailing probes never get observed (the wedge never clears).
    let script = r#"{
        "settle_probe": [
            [false, "https://app.example.com/", 0],
            [false, "https://app.example.com/", 0]
        ],
        "cross_origin_swap_at": [0]
    }"#;
    // Env base 4s, caller requests a 1.5s budget with until:"load". Pre-fix,
    // budget_ms never reaches the shim, so the executor uses the 4s env base per
    // phase and overruns to ≥4s regardless of the request. Post-fix, the 1.5s
    // budget is threaded and shared across all phases → ~1.5s. 3s cleanly
    // separates them (a ~3× ratio, robust to CI load).
    let (mgr, id, _udd) = make_manager_with_script_env(
        "xorigin-bound",
        script,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    let (outcome, elapsed) =
        wait_for_timed(&mgr, &id, "load", 1_500, Duration::from_secs(30)).await;

    // Contract 1 — TYPED outcome (never a transport error; the helper already
    // panics on Err). A pathological/never-recovering destination is allowed to
    // end in `timeout`, but it MUST be one of the typed settle verdicts.
    assert!(
        matches!(
            outcome.settle_outcome.as_str(),
            "reached" | "timeout" | "dom_unstable"
        ),
        "cross-origin swap must yield a typed settle_outcome, got {:?}",
        outcome.settle_outcome
    );
    // Contract 2 — the caller asked `until:"load"`; the receipt must record it
    // (readiness gated at the requested mode, pinned per FND-0025).
    assert_eq!(
        outcome.settle_until, "load",
        "settle_until must echo the requested readiness mode"
    );
    // Contract 3 — THE REGRESSION ASSERTION: bounded by ~1× the CALLER budget.
    // Pre-fix the shim ignored the 1.5s request and burned its 4s env base
    // across the VT-arm / reattach / resume phases (≥4s); post-fix the threaded
    // 1.5s budget, shared across all phases, holds it to ~1.5s. 3s separates them.
    assert!(
        elapsed < Duration::from_millis(3_000),
        "cross-origin swap wait must stay within ~1× the settle budget; took {elapsed:?} \
         — the shim re-took its per-phase budget across the VT-arm / reattach / resume \
         phases instead of sharing one deadline (the v0.15.0 request_timeout regression)",
    );

    mgr.shutdown_session("xorigin-bound").await;
}

// (c) The request_timeout path is unreachable for a completed cross-origin nav:
// even with a TIGHT caller budget (the shim's per-phase base is larger than the
// requested budget), the wait returns a typed WaitOutcome inside the budget — it
// does NOT let the multi-phase overrun blow past the caller's deadline into a
// transport timeout. (Pre-fix, budget_ms never reached the shim, so the executor
// used the full 2s env base per phase regardless of this 1s request.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_cross_origin_swap_honors_tight_budget() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [
            [false, "https://app.example.com/", 0],
            [false, "https://app.example.com/", 0]
        ],
        "cross_origin_swap_at": [0]
    }"#;
    // Env base 4s, but the caller requests a 1s budget: post-fix the shim honors
    // the threaded 1s and returns ~1s; pre-fix it ignored the request, used the
    // 4s env base per phase, and overran to ≥8s.
    let (mgr, id, _udd) = make_manager_with_script_env(
        "xorigin-tight",
        script,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    let (outcome, elapsed) =
        wait_for_timed(&mgr, &id, "load", 1_000, Duration::from_secs(30)).await;

    assert!(
        matches!(
            outcome.settle_outcome.as_str(),
            "reached" | "timeout" | "dom_unstable"
        ),
        "tight-budget cross-origin swap must yield a typed settle_outcome, got {:?}",
        outcome.settle_outcome
    );
    // The caller's 1s budget must bound the whole wait — proving budget_ms now
    // reaches the shim AND all phases share it. 3s ceiling separates the fixed
    // ~1s from the pre-fix ≥8s (4s env base × the multi-phase overrun).
    assert!(
        elapsed < Duration::from_millis(3_000),
        "a tight caller budget must bound the cross-origin settle (budget_ms must reach \
         the shim and be shared across phases); took {elapsed:?}",
    );

    mgr.shutdown_session("xorigin-tight").await;
}

// until:"load" returns `reached` as soon as the destination's load completes —
// the readiness semantics the acceptance criterion requires. Uses a same-process
// renavigation (the recovering case: the CDP session survives and observes the
// new document, exactly like a real top-level cross-origin nav whose target
// survives the process swap). Proves `until:"load"` is honored end-to-end WITHOUT
// an explicit Page.loadEventFired subscription — the `load` settle mode already
// resolves on the load-driven `readyState:"complete"` within one tick.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_until_load_reaches_on_navigated_document() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [
            [false, "http://fake.test/destination", 0],
            [true,  "http://fake.test/destination", 0],
            [true,  "http://fake.test/destination", 0]
        ],
        "renavigate_at": [0]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script_env(
        "until-load-reached",
        script,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    let (outcome, elapsed) =
        wait_for_timed(&mgr, &id, "load", 4_000, Duration::from_secs(30)).await;

    assert_eq!(
        outcome.settle_outcome, "reached",
        "until:load must resolve on the destination's completed load (got {:?}, {}ms)",
        outcome.settle_outcome, outcome.settle_ms
    );
    assert_eq!(outcome.settle_until, "load");
    // Returns promptly once loaded — nowhere near the 4s budget.
    assert!(
        elapsed < Duration::from_millis(3_000),
        "until:load must return on load, not wait out the budget; took {elapsed:?}",
    );

    mgr.shutdown_session("until-load-reached").await;
}

// A zero settle budget (the daemon computed it is already out of its per-call
// deadline) must return an IMMEDIATE typed `timeout` — never hang, never send a
// zero-duration CDP command (ship FND-0014/0020). The ShimManager passes
// budget_ms straight through, so budget_ms:0 exercises the shim's zero guard.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn wait_for_zero_budget_returns_immediate_typed_timeout() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[false, "https://app.example.com/", 0]],
        "cross_origin_swap_at": [0]
    }"#;
    let (mgr, id, _udd) = make_manager_with_script_env(
        "zero-budget",
        script,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    let (outcome, elapsed) = wait_for_timed(&mgr, &id, "load", 0, Duration::from_secs(30)).await;

    assert_eq!(
        outcome.settle_outcome, "timeout",
        "a zero budget must return a typed timeout verdict"
    );
    assert_eq!(outcome.settle_until, "load");
    assert!(
        elapsed < Duration::from_millis(1_000),
        "zero budget must return immediately (no CDP, no resume); took {elapsed:?}",
    );

    mgr.shutdown_session("zero-budget").await;
}

// ─────────────────────────────────────────────────────────────────────────────
// web.click DISPATCH bounding (cross-origin process-swap ack-loss).
//
// #288 bounded the post-action SETTLE (`wait_for`), but the trusted-click
// DISPATCH still passes `budget_ms=0`, so its CDP recv floors to
// `config.recv_timeout_ms`. When a click commits a cross-origin top-level
// navigation, the renderer process swaps and the stale session never gets the
// `Input.dispatchMouseEvent` ack — so the DISPATCH dead-waits the full recv
// timeout and the caller sees a transport `shim_timeout` ("the click failed",
// FALSE — the nav succeeded). These tests drive `send_trusted_click` (the
// dispatch path, distinct from the `send_wait_for` settle path above) with the
// fake's `swallow_dispatch_ack` mode.
// ─────────────────────────────────────────────────────────────────────────────

/// Like `make_manager_with_script_env`, but ALSO writes a DOM fixture
/// (`LOOM_FAKE_CHROMIUM_FIXTURE`) so `DOM.querySelector`/`getBoxModel` resolve a
/// hittable node — required for any trusted-click test. Without a fixture the
/// fake returns nodeId 0 → `SelectorNotFound`, short-circuiting before the mouse
/// dispatch is ever reached.
fn make_manager_with_click_fixture(
    session_label: &str,
    script_json: &str,
    fixture_json: &str,
    extra_env: Vec<(String, String)>,
) -> (std::sync::Arc<ShimManager>, ShimId, tempfile::TempDir) {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let script_path = user_data_dir.path().join("settle_script.json");
    std::fs::write(&script_path, script_json).expect("write settle script");
    let fixture_path = user_data_dir.path().join("dom_fixture.json");
    std::fs::write(&fixture_path, fixture_json).expect("write dom fixture");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId(format!("chromium:{session_label}"));
    let mut env = vec![
        ("LOOM_SHIM_CHROMIUM_PATH".to_string(), fake_chromium_bin()),
        (
            "LOOM_SHIM_USER_DATA_DIR".to_string(),
            user_data_dir.path().display().to_string(),
        ),
        (
            "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".to_string(),
            user_data_dir.path().display().to_string(),
        ),
        (
            "LOOM_FAKE_CHROMIUM_SCRIPT".to_string(),
            script_path.display().to_string(),
        ),
        (
            "LOOM_FAKE_CHROMIUM_FIXTURE".to_string(),
            fixture_path.display().to_string(),
        ),
    ];
    env.extend(extra_env);
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_bin().into(),
            args: vec![],
            env,
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 30_000,
            recv_timeout_ms: 60_000,
        },
    );
    (mgr, id, user_data_dir)
}

/// Drive one trusted click, measuring the wall-clock it consumed. The OUTER
/// guard panics if the dispatch hangs past it — the never-hangs guarantee for
/// the click path. An `Err` (transport shim_timeout) also panics: a click whose
/// navigation succeeded must never surface a transport error.
async fn trusted_click_timed(
    mgr: &std::sync::Arc<ShimManager>,
    id: &ShimId,
    selector: &str,
    budget_ms: u64,
    outer_timeout: Duration,
) -> (loom_host::shim_manager::InputDispatchOutcome, Duration) {
    let start = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        outer_timeout,
        mgr.send_trusted_click(id.clone(), 0, 0, selector.to_string(), budget_ms),
    )
    .await
    .expect(
        "send_trusted_click exceeded the OUTER guard — a click whose cross-origin \
         navigation swallowed the dispatch ack must return a bounded TYPED outcome \
         inside its budget, never dead-wait the recv timeout (never-hangs guarantee)",
    )
    .expect(
        "send_trusted_click returned a transport ERROR for a click whose navigation \
         succeeded — the caller would read this as 'the click failed', which is FALSE; \
         a dispatched click must return a typed InputDispatchOutcome, never Err",
    );
    (outcome, start.elapsed())
}

// A hittable node for the click selector `#go`.
const CLICK_FIXTURE: &str =
    r##"{ "boxes": { "#go": [10.0, 10.0, 110.0, 40.0] }, "viewport": [1024, 768] }"##;

// THE REGRESSION (dispatch path): a trusted click whose cross-origin navigation
// swaps the renderer swallows the mouse-dispatch ack. Pre-fix, the dispatch's
// `budget_ms=0` floors the recv to `recv_timeout_ms` (60s here) → dead-wait →
// the OUTER guard fires = RED. Post-fix, the threaded budget bounds the dispatch
// to ~1x budget and it returns a typed "dispatched" outcome (the mouse event WAS
// sent), never a transport error.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_cross_origin_swap_dispatch_is_bounded_and_typed() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[true, "https://checkout.example.com/", 0]],
        "swallow_dispatch_ack": true
    }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-xorigin-dispatch", script, CLICK_FIXTURE, vec![]);

    // Caller budget 1.5s, outer guard 15s. Pre-fix the dispatch ignores the
    // budget and floors recv to 60s → the guard fires. Post-fix the 1.5s budget
    // bounds it → ~1.5s.
    let (outcome, elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 1_500, Duration::from_secs(15)).await;

    // Bounded near the 1.5s dispatch budget — NOT the 60s recv floor / 15s guard.
    // The 6s bound leaves margin for parallel-test load while still proving the
    // dead-wait is gone (pre-fix hit the 15s outer guard).
    assert!(
        elapsed < Duration::from_millis(6_000),
        "cross-origin click dispatch must return within ~1x its budget, not dead-wait \
         the recv timeout; took {elapsed:?}",
    );
    // The element resolved + was hittable and the mouse event WAS sent; only the
    // ack was lost to the swap — a performed-but-ack-pending click, NOT a resolve/
    // hit failure and NOT a transport error (the helper already panics on `Err`).
    assert_eq!(
        outcome,
        loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending,
        "a click whose cross-origin nav swallowed the dispatch ack must report a typed \
         'dispatched' outcome (the click WAS performed), got {outcome:?}",
    );

    mgr.shutdown_session("click-xorigin-dispatch").await;
}

// Same-origin fast-return: a click that does NOT navigate acks normally, so the
// dispatch returns `Ok` promptly — v0.15.0 parity, no 30s hang. Guards against
// the bounded-dispatch change slowing the common case.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_same_origin_acks_promptly() {
    assert_binaries_built();
    // No swallow_dispatch_ack → the fake acks every mouse event immediately.
    let script = r#"{ "settle_probe": [[true, "https://app.example.com/", 0]] }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-same-origin", script, CLICK_FIXTURE, vec![]);

    // Generous budget so the (fast, always-acked) events never hit the bound
    // under parallel test load — the point is `Ok`, returned promptly.
    let (outcome, elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 10_000, Duration::from_secs(15)).await;

    assert_eq!(
        outcome,
        loom_host::shim_manager::InputDispatchOutcome::Ok,
        "a normal same-origin click must report Ok (ack received), got {outcome:?}",
    );
    assert!(
        elapsed < Duration::from_millis(5_000),
        "a normal click must return promptly (ack in tens of ms), nowhere near the 10s \
         budget; took {elapsed:?}",
    );

    mgr.shutdown_session("click-same-origin").await;
}

// THE FULL web.click SEQUENCE across a cross-origin swap: the dispatch loses its
// ack (bounded → DispatchedAckPending) AND the post-action settle observes the
// swap. Composing `send_trusted_click` + `send_wait_for` mirrors the daemon's
// dispatch-then-settle. The WHOLE verb must stay bounded and every leg typed —
// never a transport error (the "request_timeout unreachable for a completed
// click" acceptance).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_swap_then_settle_is_bounded_and_typed() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[false, "https://checkout.example.com/", 0]],
        "swallow_dispatch_ack": true,
        "cross_origin_swap_at": [0]
    }"#;
    let (mgr, id, _udd) = make_manager_with_click_fixture(
        "click-swap-settle",
        script,
        CLICK_FIXTURE,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    // Dispatch: bounded, typed "dispatched" (ack lost to the swap).
    let (dispatch_outcome, d_elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 1_500, Duration::from_secs(15)).await;
    assert_eq!(
        dispatch_outcome,
        loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending,
    );

    // Settle: bounded, typed verdict for the (never-recovering) destination.
    let (settle, s_elapsed) =
        wait_for_timed(&mgr, &id, "load", 1_500, Duration::from_secs(30)).await;
    assert!(
        matches!(
            settle.settle_outcome.as_str(),
            "reached" | "timeout" | "dom_unstable"
        ),
        "the post-click settle must yield a typed verdict, got {:?}",
        settle.settle_outcome
    );
    assert_eq!(settle.settle_until, "load");

    // The WHOLE verb (dispatch + settle) stays well inside a bounded window — the
    // caller never sees a transport request_timeout for a completed click.
    // dispatch (~1.5s budget) + settle (~1.5s budget) + parallel-load overhead —
    // 10s bound proves the whole verb is bounded (pre-fix: a 60s dispatch dead-wait).
    assert!(
        d_elapsed + s_elapsed < Duration::from_secs(10),
        "the full click sequence must stay bounded; dispatch {d_elapsed:?} + settle {s_elapsed:?}",
    );

    mgr.shutdown_session("click-swap-settle").await;
}

// Destination REACHABLE: the dispatch ack is lost (bounded → DispatchedAckPending)
// but the destination is observable (no process swap for the settle), so the
// post-action settle reaches `load`. Demonstrates "destination reachable" when a
// re-attach succeeds — the click is performed AND readiness confirmed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_swap_then_settle_reaches_recoverable_destination() {
    assert_binaries_built();
    // swallow the dispatch ack, but the settle probe reports ready (the
    // destination is observable — no cross_origin_swap_at gating the settle).
    let script = r#"{
        "settle_probe": [[true, "https://checkout.example.com/", 0]],
        "swallow_dispatch_ack": true
    }"#;
    let (mgr, id, _udd) = make_manager_with_click_fixture(
        "click-swap-reachable",
        script,
        CLICK_FIXTURE,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    let (dispatch_outcome, _d) =
        trusted_click_timed(&mgr, &id, "#go", 1_500, Duration::from_secs(15)).await;
    assert_eq!(
        dispatch_outcome,
        loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending,
    );

    let (settle, _s) = wait_for_timed(&mgr, &id, "load", 2_000, Duration::from_secs(30)).await;
    assert_eq!(
        settle.settle_outcome, "reached",
        "an observable destination must settle to 'reached', got {:?}",
        settle.settle_outcome
    );

    mgr.shutdown_session("click-swap-reachable").await;
}

// REGRESSION GUARD (ship-review): a delayed-but-ARRIVING mouse-ack (realistic
// real-Chrome browser-process latency) that lands WITHIN the dispatch budget must
// return `Ok` — NOT a spurious `DispatchedAckPending`. The fake usually acks
// in-process instantly, masking a budget collapsed below real ack latency; this
// injects a 150ms ack delay against a 1.5s budget so the ack wins the race, as a
// correctly-sized dispatch budget must allow. (The daemon's budget formula is
// unit-guarded separately in `interaction_dispatch_budget_leaves_room_for_a_real_ack`.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_delayed_ack_within_budget_is_ok() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[true, "https://app.example.com/", 0]],
        "dispatch_ack_delay_ms": 150
    }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-delayed-ack", script, CLICK_FIXTURE, vec![]);

    // 1.5s budget ≫ the 150ms ack delay → the ack arrives → Ok.
    let (outcome, elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 1_500, Duration::from_secs(15)).await;

    assert_eq!(
        outcome,
        loom_host::shim_manager::InputDispatchOutcome::Ok,
        "an ack that arrives within the dispatch budget must be Ok, not a spurious \
         DispatchedAckPending, got {outcome:?}",
    );
    // Returned around the 150ms ack, nowhere near the 1.5s budget.
    assert!(
        elapsed < Duration::from_millis(1_500),
        "must return when the ack arrives (~150ms), not wait out the budget; took {elapsed:?}",
    );

    mgr.shutdown_session("click-delayed-ack").await;
}

// ─────────────────────────────────────────────────────────────────────────────
// web.click DISPATCH bounding — PRE-COMMIT ack loss (swap opens at/before move).
//
// The surviving v0.15.2 defect (this feature): `swallow_dispatch_ack` above models
// a swap that swallows only the COMMITTING (`mousePressed`) ack, which the
// commit-index heuristic already tolerates. But a real cross-origin process swap
// can open at/before the `mouseMoved` frame and drop the PRE-COMMIT ack too. On
// v0.15.2 that lands in `dispatch_input_events`' eager `input dispatch ack timeout
// before commit` arm → a hard `shim_timeout` → a FALSE `click_failed`, even though
// the click's navigation actually started. The `swallow_dispatch_ack_from_move`
// knob withholds EVERY trusted-click mouse-frame ack, exercising that arm.
// ─────────────────────────────────────────────────────────────────────────────

// THE REGRESSION (pre-commit ack loss): a trusted click whose cross-origin swap
// opens at/before the move swallows the `mouseMoved` ack. Pre-fix → the "before
// commit" hard error (the helper panics on the resulting `Err`). Post-fix → the
// host keeps dispatching the committing frames and returns a typed "dispatched"
// outcome (the click WAS performed), bounded by ~1× budget.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_swap_at_move_dispatch_is_dispatched_not_before_commit() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[true, "https://checkout.example.com/", 0]],
        "swallow_dispatch_ack_from_move": true
    }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-swap-at-move", script, CLICK_FIXTURE, vec![]);

    // Caller budget 1.5s, outer guard 15s. Pre-fix returns Err("…before commit")
    // → the helper panics. Post-fix returns DispatchedAckPending inside ~1×budget.
    let (outcome, elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 1_500, Duration::from_secs(15)).await;

    // The WHOLE sequence is bounded by ONE shared deadline even though it now
    // traverses the timed-out pre-commit frame — NOT 2×/3× the budget.
    assert!(
        elapsed < Duration::from_millis(6_000),
        "pre-commit swap click must stay within ~1× its budget (one shared deadline \
         across all mouse frames), not multiply per timed-out frame; took {elapsed:?}",
    );
    assert_eq!(
        outcome,
        loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending,
        "a click whose cross-origin swap swallowed even the PRE-COMMIT (mouseMoved) ack \
         must report the typed 'dispatched' outcome (the committing frames were still \
         dispatched — the click WAS performed), never a transport shim_timeout, got {outcome:?}",
    );

    mgr.shutdown_session("click-swap-at-move").await;
}

// The FULL web.click across a pre-commit swap: the dispatch loses even the move
// ack (→ DispatchedAckPending) AND the post-action settle observes the swap. The
// whole verb stays bounded and every leg typed — never a transport error. This is
// the acceptance shape: click performed + bounded destination settle_outcome.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_swap_at_move_then_settle_is_bounded_and_typed() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[false, "https://checkout.example.com/", 0]],
        "swallow_dispatch_ack_from_move": true,
        "cross_origin_swap_at": [0]
    }"#;
    let (mgr, id, _udd) = make_manager_with_click_fixture(
        "click-swap-at-move-settle",
        script,
        CLICK_FIXTURE,
        vec![("LOOM_SHIM_CDP_TIMEOUT_MS".to_string(), "4000".to_string())],
    );

    let (dispatch_outcome, d_elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 1_500, Duration::from_secs(15)).await;
    assert_eq!(
        dispatch_outcome,
        loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending,
    );

    let (settle, s_elapsed) =
        wait_for_timed(&mgr, &id, "load", 1_500, Duration::from_secs(30)).await;
    assert!(
        matches!(
            settle.settle_outcome.as_str(),
            "reached" | "timeout" | "dom_unstable"
        ),
        "the post-click settle must yield a typed verdict, got {:?}",
        settle.settle_outcome
    );
    assert_eq!(settle.settle_until, "load");
    assert!(
        d_elapsed + s_elapsed < Duration::from_secs(10),
        "the full pre-commit-swap click sequence must stay bounded; dispatch {d_elapsed:?} \
         + settle {s_elapsed:?}",
    );

    mgr.shutdown_session("click-swap-at-move-settle").await;
}

// REGRESSION GUARD (c): a click that genuinely fails to dispatch still reports a
// typed FAILURE — the pre-commit tolerance must NOT mask a never-resolved target.
// An unresolvable selector (no node) short-circuits to `SelectorNotFound` BEFORE
// any mouse frame is sent, so it can never be mislabeled `DispatchedAckPending`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_unresolvable_selector_still_reports_failure() {
    assert_binaries_built();
    // Swap-at-move is armed, but it is irrelevant: the selector never resolves, so
    // the dispatch loop is never entered. Proves genuine failures stay typed
    // failures even under the pre-commit-tolerant path.
    let script = r#"{
        "settle_probe": [[true, "https://app.example.com/", 0]],
        "swallow_dispatch_ack_from_move": true
    }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-unresolvable", script, CLICK_FIXTURE, vec![]);

    // `#missing` has no box in CLICK_FIXTURE → resolve returns no node.
    let (outcome, elapsed) =
        trusted_click_timed(&mgr, &id, "#missing", 1_500, Duration::from_secs(15)).await;

    assert_eq!(
        outcome,
        loom_host::shim_manager::InputDispatchOutcome::SelectorNotFound,
        "an unresolvable selector must report SelectorNotFound, never a false \
         DispatchedAckPending, got {outcome:?}",
    );
    assert!(
        elapsed < Duration::from_millis(5_000),
        "a never-resolved selector must return promptly, not wait out the budget; took {elapsed:?}",
    );

    mgr.shutdown_session("click-unresolvable").await;
}

// A dropped PRE-COMMIT (`mouseMoved`) ack on a LIVE session (the committing frames
// still ack) must NOT degrade the outcome: the click committed and its commit ack
// arrived, so the result is a clean `Ok`, never a spurious `DispatchedAckPending`.
// (Guards against reporting a fully-committed click as ack-pending just because an
// irrelevant earlier move ack was dropped.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_move_ack_dropped_but_committed_is_ok() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[true, "https://app.example.com/", 0]],
        "swallow_dispatch_ack_move_only": true
    }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-move-ack-dropped", script, CLICK_FIXTURE, vec![]);

    let (outcome, elapsed) =
        trusted_click_timed(&mgr, &id, "#go", 2_000, Duration::from_secs(15)).await;

    assert_eq!(
        outcome,
        loom_host::shim_manager::InputDispatchOutcome::Ok,
        "a dropped mouseMoved ack with clean committing acks must report Ok (the click \
         committed), not a spurious DispatchedAckPending, got {outcome:?}",
    );
    // The move ack is lost (waits out its slice), but press/release ack promptly;
    // the whole sequence stays within the shared 2s budget.
    assert!(
        elapsed < Duration::from_millis(4_000),
        "one dropped move ack must not blow the shared dispatch budget; took {elapsed:?}",
    );

    mgr.shutdown_session("click-move-ack-dropped").await;
}

// REGRESSION GUARD (c, in-loop): a GENUINE CDP application error on the committing
// frame (renderer rejects the input) must stay a hard FAILURE — the pre-commit
// tolerance must not swallow the `Some(Err)` arm of `dispatch_input_events`. Unlike
// an ack loss (swap), a CDP error means the input was rejected, not merely un-acked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn trusted_click_cdp_error_on_commit_still_fails() {
    assert_binaries_built();
    let script = r#"{
        "settle_probe": [[true, "https://app.example.com/", 0]],
        "dispatch_cdp_error": true
    }"#;
    let (mgr, id, _udd) =
        make_manager_with_click_fixture("click-cdp-error", script, CLICK_FIXTURE, vec![]);

    // Call send_trusted_click directly — a genuine CDP dispatch error is an `Err`,
    // so the `trusted_click_timed` helper (which panics on Err) can't be used here.
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        mgr.send_trusted_click(id.clone(), 0, 0, "#go".to_string(), 1_500),
    )
    .await
    .expect("send_trusted_click must return (bounded) even on a CDP error, not hang");

    assert!(
        result.is_err(),
        "a genuine CDP application error on the committing frame must be a hard failure, \
         got {result:?}",
    );

    mgr.shutdown_session("click-cdp-error").await;
}
