use super::*;

#[tokio::test]
async fn shutdown_signal_resolves_on_sigterm() {
    let task = tokio::spawn(shutdown_signal());
    // Let the spawned future poll once so the handlers are registered
    // before the signal is raised.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    unsafe {
        libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("shutdown future must resolve on SIGTERM")
        .expect("shutdown future must not panic");
}

#[tokio::test]
async fn shutdown_signal_resolves_on_sigint() {
    let task = tokio::spawn(shutdown_signal());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    unsafe {
        libc::kill(std::process::id() as libc::pid_t, libc::SIGINT);
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("shutdown future must resolve on SIGINT")
        .expect("shutdown future must not panic");
}

#[tokio::test]
async fn abort_session_raw_spawns_shim_teardown() {
    let tmp = test_scratch_dir("abort-teardown");
    let bridge = make_bridge(&tmp);
    let sid = create_session_via(&bridge);

    bridge.abort_session_raw(&sid, "test-abort").expect("abort");

    // The teardown task must be in the JoinSet (completed tasks stay in
    // `len()` until joined, and `spawn_shim_teardown` reaps BEFORE the
    // fresh spawn — so exactly this abort's task is observable here).
    assert_eq!(
        bridge.cleanup_tasks.lock().unwrap().len(),
        1,
        "abort must spawn host.shutdown_session into cleanup_tasks \
             (browser + ShimManager entry reclamation)"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn close_session_raw_spawns_shim_teardown() {
    let tmp = test_scratch_dir("close-teardown");
    let bridge = make_bridge(&tmp);
    let sid = create_session_via(&bridge);

    bridge.close_session_raw(&sid).expect("close");

    assert_eq!(
        bridge.cleanup_tasks.lock().unwrap().len(),
        1,
        "close must spawn host.shutdown_session into cleanup_tasks"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Acceptance (typed-capacity-errors): saturating the cap yields the
/// typed `session_cap_exceeded` rejection with `{active, cap, hint}`
/// context (never the opaque internal catch-all); closing one session
/// frees a slot and create succeeds again.
// `max_concurrent_sessions()` caches `LOOM_MAX_CONCURRENT_SESSIONS` in a
// process-wide OnceLock. That makes this test irreducibly process-global:
// a sibling session test that initializes the cache first would defeat the
// pin, and this test's pin would cap every sibling. A per-test ENV_LOCK
// can't fix a *cache* (only a fresh process can), so this test runs ONLY
// under nextest's process-per-test isolation (`--run-ignored all` in CI).
// Default `cargo test` skips it (ignored) — keeping the threaded run green
// without this test polluting the shared cache.
#[ignore = "process-global env cache (LOOM_MAX_CONCURRENT_SESSIONS OnceLock) — run under nextest --run-ignored, not threaded cargo test"]
#[tokio::test]
async fn create_session_raw_cap_hit_is_typed_and_recovers_after_close() {
    // Pin the cap low. Under nextest this is a fresh process, so the OnceLock
    // latches exactly 2; assert that loudly so an accidental un-isolated run
    // (e.g. `cargo test -- --include-ignored` at default parallelism) FAILS
    // visibly instead of silently saturating the wrong cap.
    std::env::set_var("LOOM_MAX_CONCURRENT_SESSIONS", "2");
    let cap = max_concurrent_sessions();
    assert_eq!(
        cap, 2,
        "this test requires process isolation: the LOOM_MAX_CONCURRENT_SESSIONS \
             OnceLock latched {cap}, not 2 — run it via `cargo nextest run --run-ignored all`, \
             not threaded `cargo test --include-ignored`"
    );
    let tmp = test_scratch_dir("cap-typed-error");
    let bridge = make_bridge(&tmp);

    let mut sids = Vec::new();
    for _ in 0..cap {
        sids.push(create_session_via(&bridge));
    }

    let err = bridge
        .create_session_raw(params_safe())
        .expect_err("create beyond the cap must be rejected");
    assert_eq!(
        err.code,
        loom_core::error::LoomErrorCode::SessionCapExceeded,
        "cap rejection must be the typed code, got: {err}"
    );
    assert!(
        err.message.contains(&format!("({cap}/{cap})")),
        "message must carry active/cap: {}",
        err.message
    );
    let ctx = err.context.expect("cap rejection must carry context");
    assert_eq!(ctx["active"].as_u64(), Some(cap as u64));
    assert_eq!(ctx["cap"].as_u64(), Some(cap as u64));
    assert!(
        ctx["hint"]
            .as_str()
            .is_some_and(|h| h.contains("loom session reap")),
        "hint must name the remediation; got: {ctx}"
    );

    // Close one → a slot frees → create succeeds again.
    bridge
        .close_session_raw(&sids[0])
        .expect("close must succeed");
    let _ = bridge
        .create_session_raw(params_safe())
        .expect("create after freeing a slot must succeed");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Refactor guard (cleanup-create-session-params): `create_session_raw` now takes the wire
/// `CreateSessionParams` struct by value. This asserts every field still threads onto the
/// created `Session` using DISTINCT non-default values, so a field-swap mis-map — which the
/// all-defaults call sites elsewhere cannot catch — fails loudly. Two creates cover both
/// determinism bools for threading AND the `no_blocklist`↔`no_determinism` swap.
#[tokio::test]
async fn create_session_raw_threads_param_fields() {
    use loom_core::manifest_writer::SessionId;
    use loom_shared::types::Seed;

    let tmp = test_scratch_dir("threads-param-fields");
    let bridge = make_bridge(&tmp);

    // create A: seed + no_blocklist set, no_determinism clear, explicit profile.
    let (sid_a, _) = bridge
        .create_session_raw(CreateSessionParams {
            profile: "safe".to_string(),
            network_mode: "live".to_string(),
            capture_policy: None,
            seed: Some(99),
            budget: None,
            no_blocklist: true,
            no_determinism: false,
            clock_anchor: None,
            record_screencast: true,
            audio: true,
        })
        .expect("create A");
    let sess_a = bridge
        .core
        .session_manager
        .get(SessionId(sid_a))
        .expect("get session A");
    assert_eq!(sess_a.seed, Seed(99), "seed must thread through the struct");
    assert!(sess_a.no_blocklist, "no_blocklist=true must thread");
    assert!(
        !sess_a.no_determinism,
        "no_determinism=false must thread (catches a no_blocklist↔no_determinism swap)"
    );
    assert_eq!(sess_a.profile, "safe", "profile must thread");
    assert!(
        sess_a.record_screencast,
        "record_screencast=true must thread (field added by the screencast feature merge)"
    );
    assert!(
        sess_a.audio,
        "audio=true must thread (voice-call-io --audio opt-in reaches Session.audio)"
    );

    // create B: mirror — flips both bools, proving no_determinism threads to `true` too.
    let (sid_b, _) = bridge
        .create_session_raw(CreateSessionParams {
            profile: "safe".to_string(),
            network_mode: "live".to_string(),
            capture_policy: None,
            seed: None,
            budget: None,
            no_blocklist: false,
            no_determinism: true,
            clock_anchor: None,
            record_screencast: false,
            audio: false,
        })
        .expect("create B");
    let sess_b = bridge
        .core
        .session_manager
        .get(SessionId(sid_b))
        .expect("get session B");
    assert!(!sess_b.no_blocklist, "no_blocklist=false must thread");
    assert!(sess_b.no_determinism, "no_determinism=true must thread");
    assert!(
        !sess_b.record_screencast,
        "record_screencast=false must thread"
    );
    assert!(!sess_b.audio, "audio=false must thread");

    let _ = std::fs::remove_dir_all(&tmp);
}
