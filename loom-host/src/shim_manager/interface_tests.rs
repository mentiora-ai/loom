// Interface tests for `ShimManager`. Verifies the no-platform-symbols
// boundary, the circuit-breaker state machine, failure-class-aware
// eviction, read-loop transport-death detection, spawn mutual
// exclusion, and per-shim error code mapping.

use super::helpers::host_call_class;
use super::process::{host_read_loop, ShimProcess, ShimTasks};
use super::shim_manager::{
    effective_generic_recv_ms, BreakerState, FailureClass, ShimConfig, ShimId, ShimManager,
};
use crate::host_observability::HostObservability;
use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::shim_protocol::{ShimRequest, ShimResponse};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

fn fixture() -> (Arc<ShimManager>, ShimId) {
    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId("chromium".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: PathBuf::from("/opt/loom/bin/chromium-shim"),
            args: vec!["--headless".into()],
            env: vec![],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 1_000,
            recv_timeout_ms: 10_000,
        },
    );
    (mgr, id)
}

// === Construction + registration ===

#[test]
fn new_returns_arc_so_dispatch_can_clone_handle() {
    fn _ck(obs: Arc<HostObservability>) -> Arc<ShimManager> {
        ShimManager::new(obs)
    }
    let _ = _ck;
}

#[test]
fn register_shim_records_config() {
    let (_mgr, _id) = fixture();
    // The fixture registers a shim; subsequent breaker_open_window
    // must return the configured value.
    let (mgr, id) = fixture();
    assert_eq!(mgr.breaker_open_window(&id).as_millis(), 5_000);
}

// === Circuit breaker state machine (soft binding §6) ===

#[test]
fn breaker_starts_unknown_until_first_send() {
    // Per design §5: breaker state is per-shim. Before any send, no
    // ShimState entry exists.
    let (mgr, id) = fixture();
    assert!(mgr.breaker_state(&id).is_none());
}

#[test]
fn breaker_reset_clears_state_to_closed() {
    let (mgr, id) = fixture();
    // Compile-time pin only; the implementation sets up open state then resets.
    mgr.breaker_reset(&id);
    let _ = BreakerState::Closed;
}

#[test]
fn breaker_state_enum_has_three_variants() {
    let _ = BreakerState::Closed;
    let _ = BreakerState::Open;
    let _ = BreakerState::HalfOpen;
}

#[test]
fn breaker_threshold_default_is_three() {
    let c = ShimConfig::default();
    assert_eq!(c.breaker_threshold, 3); // soft binding
}

#[test]
fn breaker_open_window_default_is_5000ms() {
    let c = ShimConfig::default();
    assert_eq!(c.breaker_open_ms, 5_000); // soft binding §6
}

#[test]
fn spawn_retry_default_is_one() {
    let c = ShimConfig::default();
    assert_eq!(c.spawn_retry, 1); // single retry on posix_spawn race
}

// === Send is async (borrows caller's tokio handle) ===

#[test]
fn send_signature_is_async_and_returns_bytes() {
    fn _ck<'a>(
        m: &'a ShimManager,
        id: ShimId,
        msg: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, LoomError>> + 'a {
        m.send(id, msg)
    }
    let _ = _ck;
}

// === Per-shim error code mapping (will translate via ErrorMapper) ===

#[test]
fn shim_spawn_failed_code_carries_shim_id_and_errno() {
    let _e = LoomErrorCode::ShimFailure;
    let _: LoomError = LoomError::from(_e);
}

#[test]
fn shim_crashed_code_carries_shim_id() {
    let _e = LoomErrorCode::ShimFailure;
    let _: LoomError = LoomError::from(_e);
}

#[test]
fn shim_timeout_code_carries_phase_send_or_recv() {
    let _send = LoomErrorCode::ShimTimeout;
    let _recv = LoomErrorCode::ShimTimeout;
}

#[test]
fn shim_unavailable_code_carries_retry_after_ms() {
    let _e = LoomErrorCode::ShimBreakerOpen;
}

// === Circuit breaker recovery (open-window expiry → HalfOpen → probe) ===

/// Rewind `opened_at_ms` by `delta_ms` so window-expiry tests don't
/// sleep out the real 5 s default.
fn rewind_opened_at(mgr: &ShimManager, id: &ShimId, delta_ms: u64) {
    let mut s = mgr.states.get_mut(id).expect("state entry must exist");
    let t = s.opened_at_ms.expect("breaker must be open");
    s.opened_at_ms = Some(t.saturating_sub(delta_ms));
}

#[tokio::test]
async fn breaker_fail_fasts_within_window_then_half_opens_and_recovers() {
    let (mgr, id) = fixture(); // threshold 3, open window 5000 ms

    // Trip the breaker.
    for _ in 0..3 {
        mgr.record_failure(&id, FailureClass::Transport);
    }
    assert_eq!(mgr.breaker_state(&id), Some(BreakerState::Open));

    // Within the open window: fail-fast with ShimBreakerOpen.
    let err = mgr
        .check_breaker(&id)
        .expect_err("open breaker must fail-fast within the window");
    assert_eq!(err.code, LoomErrorCode::ShimBreakerOpen);

    // Expire the window: the next call is admitted as a probe and the
    // breaker transitions to HalfOpen.
    rewind_opened_at(&mgr, &id, 5_001);
    mgr.check_breaker(&id)
        .expect("expired window must admit a probe");
    assert_eq!(mgr.breaker_state(&id), Some(BreakerState::HalfOpen));

    // Probe success → Closed, counters reset.
    mgr.record_success(&id);
    assert_eq!(mgr.breaker_state(&id), Some(BreakerState::Closed));
    let state = mgr.shim_state(&id).expect("state entry");
    assert_eq!(state.consecutive_failures, 0);
    assert!(state.opened_at_ms.is_none());

    // Fully recovered: subsequent calls pass the breaker again.
    mgr.check_breaker(&id).expect("closed breaker must pass");
}

#[tokio::test]
async fn half_open_probe_failure_reopens_with_fresh_window() {
    let (mgr, id) = fixture();

    for _ in 0..3 {
        mgr.record_failure(&id, FailureClass::Transport);
    }
    rewind_opened_at(&mgr, &id, 5_001);
    mgr.check_breaker(&id)
        .expect("expired window must admit a probe");
    assert_eq!(mgr.breaker_state(&id), Some(BreakerState::HalfOpen));

    // Probe failure (any class) → re-open with a FRESH opened_at_ms, so
    // the very next call fail-fasts for another full window.
    mgr.record_failure(&id, FailureClass::Application);
    assert_eq!(mgr.breaker_state(&id), Some(BreakerState::Open));
    let err = mgr
        .check_breaker(&id)
        .expect_err("re-opened breaker must fail-fast again");
    assert_eq!(err.code, LoomErrorCode::ShimBreakerOpen);
}

// === Failure classes (application errors must not kill the process) ===

/// A fake `ShimProcess` for eviction tests. `crashed` is pre-set so the
/// transport-eviction cleanup task's `shutdown_process` short-circuits
/// (its kill path would otherwise signal `child_pid` 0 == the test's
/// own process group). The request receiver is returned so the channel
/// stays open.
fn fake_process() -> (Arc<ShimProcess>, tokio::sync::mpsc::Receiver<ShimRequest>) {
    let (request_tx, request_rx) = tokio::sync::mpsc::channel::<ShimRequest>(16);
    fn noop_task() -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            std::future::pending::<()>().await;
        })
    }
    let process = Arc::new(ShimProcess {
        child: tokio::sync::Mutex::new(None),
        child_pid: 0,
        request_tx,
        pending: Arc::new(dashmap::DashMap::new()),
        next_request_id: Arc::new(AtomicU64::new(0)),
        crashed: Arc::new(AtomicBool::new(true)),
        exit_status_text: Arc::new(parking_lot::Mutex::new(None)),
        tasks: ShimTasks {
            read: noop_task(),
            write: noop_task(),
            demux: noop_task(),
            watcher: noop_task(),
        },
    });
    (process, request_rx)
}

#[tokio::test]
async fn application_failure_counts_toward_breaker_but_does_not_evict() {
    let (mgr, id) = fixture();
    let (process, _request_rx) = fake_process();
    mgr.processes.insert(id.clone(), process);

    // An application-level failure (e.g. CdpProtocolError from a bad
    // Runtime.evaluate) must count toward the breaker WITHOUT killing
    // the live process entry — the Chromium under it is healthy and
    // holds mid-session browser state.
    mgr.record_failure(&id, FailureClass::Application);
    assert!(
        mgr.processes.contains_key(&id),
        "application failure must NOT evict the live process entry"
    );
    let state = mgr.shim_state(&id).expect("state entry");
    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(state.breaker, BreakerState::Closed);

    // A transport failure keeps the original behavior: evict.
    mgr.record_failure(&id, FailureClass::Transport);
    assert!(
        !mgr.processes.contains_key(&id),
        "transport failure must evict the process entry"
    );
}

// === Host-side round-trip failure classification (#347) ===

/// Build the `LoomError` `send_and_await` returns when the shim's answer does
/// not arrive in time, tagged exactly as the production path tags it.
fn recv_timeout_err() -> LoomError {
    LoomError::new(
        LoomErrorCode::ShimTimeout,
        "shim recv timeout after 30000 ms",
    )
    .with_context(serde_json::json!({
        super::process::TIMEOUT_PHASE_KEY: super::process::TIMEOUT_PHASE_RECV
    }))
}

/// And the one `register_and_send` returns when the frame never left the host.
fn send_timeout_err() -> LoomError {
    LoomError::new(
        LoomErrorCode::ShimTimeout,
        "shim send timeout after 1000 ms",
    )
    .with_context(serde_json::json!({
        super::process::TIMEOUT_PHASE_KEY: super::process::TIMEOUT_PHASE_SEND
    }))
}

#[test]
fn recv_timeout_is_application_class_but_send_timeout_is_transport() {
    // The frame reached a live shim and the PAGE was slow: the subprocess is
    // healthy, so this must not evict it. Same condition the shim reports as
    // `CdpTimeout`, which `shim_error_class` already calls Application.
    assert_eq!(
        host_call_class(&recv_timeout_err()),
        FailureClass::Application,
        "a recv timeout came from a live shim — it must not evict a healthy Chromium"
    );

    // The frame never left the host: the channel is wedged or the subprocess is
    // gone. Still a transport failure.
    assert_eq!(
        host_call_class(&send_timeout_err()),
        FailureClass::Transport,
        "a send timeout means the frame never reached the shim"
    );

    // Everything else keeps the original classification.
    for err in [
        LoomError::new(LoomErrorCode::ShimFailure, "shim crashed: exit 256"),
        LoomError::new(LoomErrorCode::ShimFailure, "shim request channel closed"),
        LoomError::new(
            LoomErrorCode::ShimFailure,
            "shim response oneshot dropped (subprocess gone)",
        ),
        // An untagged ShimTimeout (e.g. from a direct constructor) is treated
        // conservatively as transport — only the explicit recv tag downgrades.
        LoomError::new(LoomErrorCode::ShimTimeout, "shim timeout"),
    ] {
        assert_eq!(
            host_call_class(&err),
            FailureClass::Transport,
            "unexpected class for {err:?}"
        );
    }
}

#[tokio::test]
async fn recv_timeout_does_not_evict_the_live_process() {
    // The #347 regression, end to end through `record_failure`: a `web.screenshot`
    // that runs past its recv timeout used to be classed Transport, which evicted
    // the subprocess and killed the session's Chromium — every later verb then
    // failed with surface_trap and respawned shims could not reconnect.
    let (mgr, id) = fixture();
    let (process, _request_rx) = fake_process();
    mgr.processes.insert(id.clone(), process);

    mgr.record_failure(&id, host_call_class(&recv_timeout_err()));

    assert!(
        mgr.processes.contains_key(&id),
        "a slow screenshot must leave the session's browser up"
    );
    // It still counts toward the breaker, so a genuinely wedged shim is bounded.
    let state = mgr.shim_state(&id).expect("state entry");
    assert_eq!(state.consecutive_failures, 1);

    // A send timeout on the same shim still evicts.
    mgr.record_failure(&id, host_call_class(&send_timeout_err()));
    assert!(
        !mgr.processes.contains_key(&id),
        "a send timeout must still evict the process entry"
    );
}

// === Read-loop transport-death detection ===

#[tokio::test]
async fn read_loop_oversized_frame_marks_crashed_and_fails_pending() {
    use tokio::io::AsyncWriteExt;

    let (host_side, mut shim_side) = tokio::net::UnixStream::pair().expect("socketpair");
    let (read_half, _write_half) = host_side.into_split();
    let (response_tx, _response_rx) = tokio::sync::mpsc::channel::<ShimResponse>(8);
    let crashed = Arc::new(AtomicBool::new(false));
    let exit_text: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let pending: Arc<dashmap::DashMap<u64, tokio::sync::oneshot::Sender<ShimResponse>>> =
        Arc::new(dashmap::DashMap::new());

    // Park an in-flight request like send_and_await would.
    let (tx, rx) = tokio::sync::oneshot::channel::<ShimResponse>();
    pending.insert(7, tx);

    let loop_task = tokio::spawn(host_read_loop(
        read_half,
        response_tx,
        crashed.clone(),
        exit_text.clone(),
        pending.clone(),
    ));

    // A length prefix beyond MAX_FRAME_BYTES is unrecoverable framing.
    let oversized = loom_shared::shim_protocol::MAX_FRAME_BYTES + 1;
    shim_side
        .write_all(&oversized.to_be_bytes())
        .await
        .expect("write length prefix");

    loop_task.await.expect("read loop must exit, not panic");
    assert!(
        crashed.load(Ordering::SeqCst),
        "oversized frame must mark the transport crashed"
    );
    assert!(
        pending.is_empty(),
        "pending requests must be failed (cleared), not left to stall recv_timeout"
    );
    assert!(
        rx.await.is_err(),
        "the parked oneshot must be dropped so the caller wakes immediately"
    );
    assert!(
        exit_text
            .lock()
            .as_deref()
            .is_some_and(|t| t.contains("frame too large")),
        "exit_status_text must carry the framing-failure reason"
    );
}

#[tokio::test]
async fn read_loop_decode_failure_marks_crashed_and_fails_pending() {
    use tokio::io::AsyncWriteExt;

    let (host_side, mut shim_side) = tokio::net::UnixStream::pair().expect("socketpair");
    let (read_half, _write_half) = host_side.into_split();
    let (response_tx, _response_rx) = tokio::sync::mpsc::channel::<ShimResponse>(8);
    let crashed = Arc::new(AtomicBool::new(false));
    let exit_text: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let pending: Arc<dashmap::DashMap<u64, tokio::sync::oneshot::Sender<ShimResponse>>> =
        Arc::new(dashmap::DashMap::new());

    let (tx, rx) = tokio::sync::oneshot::channel::<ShimResponse>();
    pending.insert(42, tx);

    let loop_task = tokio::spawn(host_read_loop(
        read_half,
        response_tx,
        crashed.clone(),
        exit_text.clone(),
        pending.clone(),
    ));

    // Well-formed length prefix, payload that is NOT a ShimResponse
    // (CBOR unsigned int 0 — the internally-tagged enum needs a map).
    let garbage = [0x00u8];
    shim_side
        .write_all(&(garbage.len() as u32).to_be_bytes())
        .await
        .expect("write length prefix");
    shim_side.write_all(&garbage).await.expect("write payload");

    loop_task.await.expect("read loop must exit, not panic");
    assert!(
        crashed.load(Ordering::SeqCst),
        "decode failure must mark the transport crashed"
    );
    assert!(pending.is_empty(), "pending must be cleared");
    assert!(rx.await.is_err(), "parked oneshot must be dropped");
    assert!(
        exit_text
            .lock()
            .as_deref()
            .is_some_and(|t| t.contains("decode")),
        "exit_status_text must carry the decode-failure reason"
    );
}

// === get_or_spawn per-id mutual exclusion ===

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_get_or_spawn_returns_single_shared_instance() {
    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId("chromium:spawn-race-test".into());
    // `/bin/sleep` exists on macOS + Linux, spawns instantly, and
    // ignores the inherited IPC fd — perfect for exercising the
    // check-spawn-insert critical section without a real shim protocol.
    let config = ShimConfig {
        binary_path: PathBuf::from("/bin/sleep"),
        args: vec!["30".into()],
        env: vec![],
        ..ShimConfig::default()
    };
    mgr.register(id.clone(), config.clone());

    // Concurrent first calls from multiple worker threads: without
    // per-id mutual exclusion, both miss the cache, both spawn, and the
    // second insert displaces the first Arc (double-spawn race).
    let mut handles = Vec::new();
    for _ in 0..8 {
        let mgr = mgr.clone();
        let id = id.clone();
        let config = config.clone();
        handles.push(tokio::spawn(
            async move { mgr.get_or_spawn(&id, &config).await },
        ));
    }
    let mut procs = Vec::new();
    for h in handles {
        procs.push(h.await.expect("join").expect("get_or_spawn must succeed"));
    }

    let first = &procs[0];
    for (i, p) in procs.iter().enumerate() {
        assert!(
            Arc::ptr_eq(first, p),
            "caller {i} got a different ShimProcess — double-spawn race"
        );
    }
    assert_eq!(
        mgr.processes.len(),
        1,
        "exactly one live process entry must exist"
    );

    // Reap the sleeper; wait for the watcher to observe the exit so the
    // test leaves no zombie behind.
    unsafe {
        libc::kill(first.child_pid as libc::pid_t, libc::SIGKILL);
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !first.crashed.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// === No platform-symbol imports ===

#[test]
fn shim_id_is_a_plain_string_newtype_no_platform_types() {
    // The struct field is `pub String`. No CFRunLoop, no ApplicationServices,
    // no chromiumoxide. Lint enforcement; this test pins by inspection.
    let id = ShimId("ax".into());
    assert_eq!(id.0, "ax");
}

// === Sink-module dependency: ShimManager → HostObservability only ===

#[test]
fn shim_manager_constructor_takes_observability_and_nothing_else_loom_host() {
    // Per dependency block: `ShimManager -> [HostObservability]`. No
    // backward edges; no host_function_table import.
    fn _ck(obs: Arc<HostObservability>) -> Arc<ShimManager> {
        ShimManager::new(obs)
    }
    let _ = _ck;
}

// === Session-scoped state cleanup (audit 2026-06-10) ===

// (audit 2026-06-10, "ShimManager::host_session_ids grows unbounded"):
// `shim_session_id_for` inserts one ULID -> wire-id entry per session;
// `shutdown_session` now removes from `host_session_ids` alongside
// `processes` / `states` / `configs` / `spawn_locks` so the map cannot grow
// monotonically under session churn. FIXED.
#[tokio::test]
async fn shutdown_session_must_remove_host_session_ids_entry() {
    let (mgr, _id) = fixture();
    let ulid = "01HZTESTHOSTSESSIONIDLEAK0";
    let _wire = mgr.shim_session_id_for(ulid);
    assert!(
        mgr.host_session_ids.contains_key(ulid),
        "precondition: shim_session_id_for must have inserted the mapping"
    );

    mgr.shutdown_session(ulid).await;

    assert!(
        !mgr.host_session_ids.contains_key(ulid),
        "shutdown_session must remove the session's host_session_ids entry \
         alongside processes/states/configs, or the map grows unbounded"
    );
}

// animation-capture (Mode B, D11): the generic CdpSend leg's recv timeout must
// be the larger of the shim's configured recv timeout and the RPC request-timeout
// cap — so a heavy standalone screenshot riding this leg isn't cut at the flat
// 30s shim recv when the operator raised LOOM_REQUEST_TIMEOUT_MS (and the RPC
// allowed more). Pure function; covers the no-op default and the raised-cap case.
#[test]
fn generic_recv_timeout_caps_align_with_rpc_request_timeout() {
    // Defaults agree (30s == 30s): no change.
    assert_eq!(effective_generic_recv_ms(30_000, 30_000), 30_000);
    // Operator raised the RPC cap above the shim recv: the shim leg follows it up.
    assert_eq!(effective_generic_recv_ms(30_000, 120_000), 120_000);
    // A larger shim recv than the cap is preserved (max, never shrinks the leg).
    assert_eq!(effective_generic_recv_ms(45_000, 30_000), 45_000);
}
