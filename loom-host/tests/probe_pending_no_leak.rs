//! K7 from #57 — locks in the invariant that `ShimProcess::pending`
//! does not accumulate entries when `send_and_await` times out.
//!
//! ## Why this exists
//!
//! `process.pending` is a `DashMap<u64, oneshot::Sender<ShimResponse>>`
//! keyed by `request_id`. Each `send_and_await` call inserts an entry
//! before writing the request frame and (in the happy path) the demux
//! loop removes it when the matching response comes back. On the
//! unhappy path — shim crashes mid-request, send-side mpsc closed,
//! recv-side timeout — the cleanup MUST happen via the explicit
//! `process.pending.remove(&request_id)` calls in each error arm of
//! `send_and_await`.
//!
//! The Step 7 finding 5(c) of #56's plan-doc identified this as a
//! load-bearing invariant: if a future regression wraps `send_and_await`
//! in a caller-side `tokio::select!` (so the future is dropped before
//! the internal timeout fires), the cleanup arms never execute and
//! `pending` leaks one entry per cancelled call. The comment at
//! `shim_manager::probe_health` documents the rule ("timeout is enforced
//! INSIDE `send_and_await`, so cancelling the returned future cannot
//! leak entries") but no automated test had pinned it down — until
//! now.
//!
//! ## What's tested
//!
//! Constructs a `ShimProcess` directly (no fake-chromium subprocess, no
//! real socketpair) by holding a `(request_tx, request_rx)` pair where
//! the receiver is parked and never drained. This guarantees that any
//! `send_and_await` call goes through:
//!   1. Insert into `pending`
//!   2. `request_tx.send(req)` succeeds (channel still open)
//!   3. The shim never responds (because there is no shim)
//!   4. The `recv_timeout` fires inside `send_and_await`
//!   5. The cleanup arm removes from `pending`
//!
//! Asserting `pending.is_empty()` after step (5) locks the invariant in.
//!
//! ## What's NOT tested
//!
//! The "future dropped by caller's `tokio::select!`" case is deliberately
//! NOT tested here. That case is the FAILURE MODE the invariant protects
//! against — the design forbids caller-level `tokio::select!`-wrapping
//! precisely BECAUSE it would leak `pending`. Asserting that it leaks
//! would just lock in current buggy behaviour. The protection lives at
//! the type/policy level (the documented "do not wrap" rule), not at
//! the runtime level.

#![cfg(unix)]

use loom_host::shim_manager::process::{send_and_await, ShimProcess, ShimTasks};
use loom_shared::error_format::LoomErrorCode;
use loom_shared::shim_protocol::ShimRequest;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Build a `ShimProcess` whose request mpsc has a never-drained
/// receiver and whose pending map starts empty. The "child" is a dummy
/// pid (this test never observes it). All four background-task
/// JoinHandles are short-lived no-ops — they only exist to satisfy the
/// `ShimTasks { read, write, demux, watcher }` shape; they're not the
/// system under test.
fn build_fake_process() -> (ShimProcess, mpsc::Receiver<ShimRequest>) {
    let (request_tx, request_rx) = mpsc::channel::<ShimRequest>(16);

    fn noop_task() -> JoinHandle<()> {
        tokio::spawn(async move {
            // Park forever; aborted when the process is dropped.
            std::future::pending::<()>().await;
        })
    }

    let process = ShimProcess {
        child: tokio::sync::Mutex::new(None),
        child_pid: 0,
        request_tx,
        pending: Arc::new(dashmap::DashMap::new()),
        next_request_id: Arc::new(AtomicU64::new(0)),
        crashed: Arc::new(AtomicBool::new(false)),
        exit_status_text: Arc::new(parking_lot::Mutex::new(None)),
        tasks: ShimTasks {
            read: noop_task(),
            write: noop_task(),
            demux: noop_task(),
            watcher: noop_task(),
        },
    };

    (process, request_rx)
}

#[tokio::test]
async fn send_and_await_recv_timeout_removes_pending_entry() {
    let (process, _request_rx) = build_fake_process();

    // 50 ms is enough for the recv path to park, the timeout to fire,
    // and the cleanup arm to remove the pending entry — without
    // blowing up the test wall-clock.
    let result = send_and_await(
        &process,
        ShimRequest::Health { request_id: 0 },
        Duration::from_millis(500), // send timeout — well clear of recv
        Duration::from_millis(50),  // recv timeout — the deadline under test
    )
    .await;

    // The call must surface as a typed `ShimTimeout`, NOT a `ShimFailure`
    // (that would indicate the send channel closed, which is a different
    // bug).
    let err = result.expect_err("send_and_await must time out on recv path");
    assert_eq!(
        err.code,
        LoomErrorCode::ShimTimeout,
        "expected ShimTimeout, got {:?}: {}",
        err.code,
        err.message
    );

    // #347: a real recv timeout must carry the `timeout_phase: "recv"` context
    // tag. The host classifies on that tag to keep a slow round-trip from
    // evicting the subprocess and killing the session's healthy Chromium. The
    // classifier itself is unit-tested in `shim_manager::interface_tests`; this
    // pins the other half — that the production error actually arrives tagged.
    // Were the tag dropped or renamed, the classifier would silently fall back
    // to the transport class and the lost-session bug would be back.
    assert_eq!(
        err.context
            .as_ref()
            .and_then(|c| c.get("timeout_phase"))
            .and_then(|v| v.as_str()),
        Some("recv"),
        "a recv timeout must be tagged as such; got context {:?}",
        err.context
    );

    // The load-bearing assertion: cleanup ran.
    assert!(
        process.pending.is_empty(),
        "pending map must be empty after recv timeout; \
         {} stale entries indicate a regression of the Step 7 5(c) invariant",
        process.pending.len(),
    );
}

#[tokio::test]
async fn send_and_await_send_channel_closed_removes_pending_entry() {
    // Drop the receiver before calling — request_tx.send() will fail
    // immediately, which exercises the send-side cleanup arm
    // (process.pending.remove on Ok(Err(_))).
    let (process, request_rx) = build_fake_process();
    drop(request_rx);

    let result = send_and_await(
        &process,
        ShimRequest::Health { request_id: 0 },
        Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .await;

    let err = result.expect_err("send must fail with closed channel");
    assert_eq!(
        err.code,
        LoomErrorCode::ShimFailure,
        "expected ShimFailure on closed send channel, got {:?}: {}",
        err.code,
        err.message
    );

    assert!(
        process.pending.is_empty(),
        "pending map must be empty after send-side closed-channel error; \
         {} stale entries",
        process.pending.len(),
    );
}

#[tokio::test]
async fn send_and_await_crashed_flag_short_circuits_without_pending_insert() {
    // The crashed-flag fast-path returns before inserting into pending
    // (process.rs lines around the `if process.crashed.load(...)` check).
    // This test pins that down: a crashed shim must not even allocate
    // a pending entry, never mind leak one.
    let (process, _request_rx) = build_fake_process();
    process
        .crashed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let result = send_and_await(
        &process,
        ShimRequest::Health { request_id: 0 },
        Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .await;

    let err = result.expect_err("crashed flag must short-circuit");
    assert_eq!(err.code, LoomErrorCode::ShimFailure);
    assert!(
        process.pending.is_empty(),
        "crashed-flag fast-path must not insert into pending; \
         {} stale entries",
        process.pending.len(),
    );
}

#[tokio::test]
async fn pending_stays_empty_across_many_consecutive_timeouts() {
    // Looped variant — exercises the cleanup invariant under N back-to-back
    // timeouts. Any miss in the cleanup arms would make pending grow
    // linearly with N; this catches that even if a single-shot test would
    // pass by coincidence (e.g. if cleanup were happening on the next
    // call instead of the current one).
    let (process, _request_rx) = build_fake_process();

    for i in 0..10 {
        let result = send_and_await(
            &process,
            ShimRequest::Health { request_id: 0 },
            Duration::from_millis(500),
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(
            result.expect_err("recv must time out").code,
            LoomErrorCode::ShimTimeout,
            "iter {i}: wrong error code",
        );
    }

    assert!(
        process.pending.is_empty(),
        "pending leaked across 10 timeouts; {} stale entries",
        process.pending.len(),
    );
}
