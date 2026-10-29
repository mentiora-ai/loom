//! Per-shim subprocess bookkeeping. Owned by `ShimManager`; one
//! `ShimProcess` per active `(ShimId, session_id)` mapping (the host
//! namespaces ShimId with the session ULID before spawn).

use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::shim_protocol::{ShimRequest, ShimResponse};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Live shim subprocess. Lifetime is owned by the ShimManager's
/// `processes` DashMap; dropping the Arc closes the channels which
/// triggers a graceful shutdown chain on the shim side.
pub struct ShimProcess {
    pub child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    pub child_pid: u32,
    pub request_tx: mpsc::Sender<ShimRequest>,
    pub pending: Arc<dashmap::DashMap<u64, oneshot::Sender<ShimResponse>>>,
    pub next_request_id: Arc<AtomicU64>,
    /// Set by the watcher task when the shim subprocess exits
    /// unexpectedly (kill -9, panic, OOM). When true, `ShimManager::send`
    /// fail-fasts with `ShimFailure` instead of writing to the dead
    /// socket and waiting for a response that will never come (which
    /// previously caused the daemon to hang for ~30s when a shim was
    /// killed mid-action).
    pub crashed: Arc<std::sync::atomic::AtomicBool>,
    /// Optional exit-status string captured by the watcher task, e.g.
    /// `"signal: 9 (SIGKILL)"` or `"exit code 1"`. Surfaces in the
    /// ShimFailure error detail so the operator can tell what happened.
    pub exit_status_text: Arc<parking_lot::Mutex<Option<String>>>,
    /// Background tasks: read loop, write loop, demux loop, and the
    /// crash watcher. Aborted on drop.
    pub tasks: ShimTasks,
}

pub struct ShimTasks {
    pub read: JoinHandle<()>,
    pub write: JoinHandle<()>,
    pub demux: JoinHandle<()>,
    pub watcher: JoinHandle<()>,
}

impl Drop for ShimTasks {
    fn drop(&mut self) {
        self.read.abort();
        self.write.abort();
        self.demux.abort();
        self.watcher.abort();
    }
}

/// Per-shim process spawn config snapshot.
#[derive(Clone)]
pub struct SpawnConfig {
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// File descriptor the IPC socketpair end is pinned to in the shim.
///
/// MUST be well clear of the low range (3-8): the shim builds a Tokio
/// multi-thread runtime at startup, and the runtime's I/O driver claims
/// the lowest free descriptors for its `epoll` instance + wakeup eventfd.
/// fd 3 (the first fd above stdio) collides with that — adopting fd 3 as
/// the IPC socket then races the I/O driver, surfacing as
/// `UnixStream::from_std: EINVAL` followed by a `Bad file descriptor`
/// panic in the driver. Pinning high (10) leaves the runtime's range free.
const SHIM_IPC_FD: RawFd = 10;

/// Spawn a shim subprocess: AF_UNIX socketpair (CLOEXEC on both ends),
/// `pre_exec` dup2 to `SHIM_IPC_FD`, and `LOOM_SHIM_FD` env. Returns a
/// `ShimProcess` with all background loops running.
///
/// The pre_exec body MUST be async-signal-safe — only `libc::dup2`,
/// `libc::fcntl`, and `libc::close` are allowed. No allocations, no
/// `format!`, no `eprintln!` (per practitioner gotcha #2).
pub async fn spawn_shim(config: &SpawnConfig) -> Result<Arc<ShimProcess>, LoomError> {
    // STEP 1: AF_UNIX SOCK_STREAM socketpair via libc, with CLOEXEC on
    // BOTH ends. Without CLOEXEC the exec'd shim inherits the host's end
    // of its own IPC socket (so its EOF-based "daemon died" detection
    // can never fire — the shim itself holds the write end open), and
    // concurrently-spawned shims inherit other sessions' in-flight fds.
    // The child still receives its end: the pre_exec dup2 to
    // `SHIM_IPC_FD` clears the flag on the new descriptor.
    //
    // Linux gets SOCK_CLOEXEC (atomic with creation — no window for a
    // concurrent spawn on another thread to fork between socketpair and
    // fcntl); macOS has no SOCK_CLOEXEC, so fall back to fcntl
    // immediately after creation.
    let mut fds = [0i32; 2];
    #[cfg(target_os = "linux")]
    let sock_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let sock_type = libc::SOCK_STREAM;
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, sock_type, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(LoomError::new(
            LoomErrorCode::ShimFailure,
            format!("socketpair failed: {errno}"),
        ));
    }
    let parent_fd: RawFd = fds[0];
    let child_fd: RawFd = fds[1];
    #[cfg(not(target_os = "linux"))]
    for fd in fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            let errno = std::io::Error::last_os_error();
            unsafe {
                libc::close(parent_fd);
                libc::close(child_fd);
            }
            return Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("fcntl(FD_CLOEXEC) failed: {errno}"),
            ));
        }
    }

    // STEP 2: build the spawn command. dup2(child_fd, SHIM_IPC_FD) in
    // pre_exec to pin the FD; LOOM_SHIM_FD carries the number to the shim.
    let mut cmd = tokio::process::Command::new(&config.binary_path);
    cmd.args(&config.args);
    cmd.env("LOOM_SHIM_FD", SHIM_IPC_FD.to_string());
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false);

    // SAFETY: pre_exec body only calls async-signal-safe libc functions.
    unsafe {
        cmd.pre_exec(move || {
            if child_fd == SHIM_IPC_FD {
                // dup2(fd, fd) is a no-op that does NOT clear FD_CLOEXEC
                // — clear it explicitly so the exec'd shim keeps its end.
                if libc::fcntl(SHIM_IPC_FD, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else {
                // dup2 clears FD_CLOEXEC on the new descriptor, so the
                // shim inherits exactly one end of the pair: fd 10.
                if libc::dup2(child_fd, SHIM_IPC_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(child_fd);
            }
            Ok(())
        });
    }

    // Pin the Chromium subprocess into a fresh process group so the
    // shim's `chromium.shutdown()` (Unix) or the watcher's `killpg`
    // can reap the entire helper-process subtree atomically. Without
    // this, `kill -9` of the shim leaves ~8 orphan Chromium helpers
    // (renderer, GPU, utility processes) running with the shim's
    // user-data-dir. (No orphans permitted.)
    //
    // We apply this via `pre_exec` AFTER the dup2/close calls because
    // setpgid is async-signal-safe (per POSIX). The shim binary will
    // then propagate the same convention to its Chromium spawn.
    //
    // For the shim subprocess itself we DON'T set a new pgid here:
    // setpgid on the SHIM means a single `killpg(shim_pgid)` from the
    // host kills the shim AND any of its descendants. Useful when the
    // host needs to force-tear-down a hung shim. But the shim's own
    // Chromium subtree is reaped via the shim-side process_group call
    // in ChromiumSupervisor::start (Part B of this PR).
    let mut child = cmd.spawn().map_err(|e| {
        unsafe {
            libc::close(parent_fd);
            libc::close(child_fd);
        }
        LoomError::new(
            LoomErrorCode::ShimFailure,
            format!("spawn shim {:?}: {e}", config.binary_path),
        )
    })?;

    // Drain the shim's piped stderr into the daemon log. The shim's stderr
    // (panic messages with `panicked at …`, `RUST_LOG` traces) was previously
    // captured by the pipe but never read — so a shim panic surfaced only as an
    // opaque `ExitStatus(256)` in the watcher, with the actual cause lost in the
    // undrained pipe (and, under verbose `RUST_LOG`, a full 64KB pipe could even
    // block the shim on write). Forward each line at WARN under a dedicated
    // target so a crash is diagnosable from the daemon log alone.
    if let Some(stderr) = child.stderr.take() {
        let shim_pid_for_stderr = child.id();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(
                    target: "loom_host::shim_stderr",
                    shim_pid = shim_pid_for_stderr,
                    "{line}"
                );
            }
        });
    }

    // STEP 3: parent closes its copy of child_fd.
    unsafe { libc::close(child_fd) };

    let child_pid = child.id().ok_or_else(|| {
        LoomError::new(
            LoomErrorCode::ShimFailure,
            "spawn returned no pid".to_string(),
        )
    })?;

    // STEP 4: wrap parent_fd as a tokio UnixStream.
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(parent_fd) };
    std_stream
        .set_nonblocking(true)
        .map_err(|e| LoomError::new(LoomErrorCode::ShimFailure, format!("set_nonblocking: {e}")))?;
    let stream = tokio::net::UnixStream::from_std(std_stream).map_err(|e| {
        LoomError::new(
            LoomErrorCode::ShimFailure,
            format!("UnixStream::from_std: {e}"),
        )
    })?;
    let (read_half, write_half) = stream.into_split();

    // Shared liveness state, created before the IO loops so the read
    // loop can mark the transport dead on a framing failure (see
    // `host_read_loop`) the same way the crash watcher does on child
    // exit.
    let pending: Arc<dashmap::DashMap<u64, oneshot::Sender<ShimResponse>>> =
        Arc::new(dashmap::DashMap::new());
    let crashed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exit_status_text: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // STEP 5: spawn the host-side framing loops.
    let (request_tx, request_rx_internal) = mpsc::channel::<ShimRequest>(64);
    let (response_tx_internal, mut response_rx) = mpsc::channel::<ShimResponse>(64);

    let write = tokio::spawn(host_write_loop(write_half, request_rx_internal));
    let read = tokio::spawn(host_read_loop(
        read_half,
        response_tx_internal,
        crashed.clone(),
        exit_status_text.clone(),
        pending.clone(),
    ));

    // STEP 6: demux loop — pulls ShimResponse and resolves the matching
    // pending oneshot by request_id. Async pushes (CdpEvent / LogLine)
    // are logged at trace level for now.
    let pending_for_demux = pending.clone();
    let demux = tokio::spawn(async move {
        while let Some(resp) = response_rx.recv().await {
            match &resp {
                ShimResponse::Ok { request_id, .. } | ShimResponse::Error { request_id, .. } => {
                    if let Some((_, tx)) = pending_for_demux.remove(request_id) {
                        let _ = tx.send(resp);
                    }
                }
                ShimResponse::CdpEvent { .. } => {
                    tracing::trace!("shim cdp_event");
                }
                ShimResponse::LogLine {
                    level,
                    target,
                    message,
                } => {
                    tracing::trace!(target = target, level = level, "{message}");
                }
            }
        }
        // response_rx closed: shim exited. All remaining pending oneshots
        // get dropped here; senders see RecvError and surface as
        // ShimFailure.
        pending_for_demux.clear();
    });

    // STEP 7: spawn the crash watcher. Without this, when the shim
    // subprocess exits unexpectedly (kill -9, panic, OOM), the host's
    // outstanding `send_and_await` oneshots just time out at the per-
    // call recv_timeout (default 30s). The watcher proactively detects
    // child exit, flips the `crashed` flag, drops all pending oneshots,
    // and closes the request channel so `send` can fail-fast.
    //
    // We move ownership of the Child handle into the watcher task —
    // wait() requires &mut Child. The ShimProcess.child Mutex<Option>
    // retains None after the watcher takes the handle;
    // shutdown_process can still kill via libc::kill(pid, SIGKILL)
    // since we cached child_pid.
    let pending_for_watcher = pending.clone();
    let request_tx_for_watcher = request_tx.clone();
    let crashed_for_watcher = crashed.clone();
    let exit_text_for_watcher = exit_status_text.clone();
    // Capture the user-data-dir env so the watcher can reap orphan
    // Chromium processes after a SIGKILL'd shim.
    // SIGKILL is uncatchable, so the shim's own SIGTERM handler can't
    // run; the host has to reap from outside the dying tree. We
    // identify the orphan tree by the unique --user-data-dir flag
    // Chromium was launched with.
    let chromium_user_data_dir: Option<String> = config
        .env
        .iter()
        .find(|(k, _)| k == "LOOM_SHIM_USER_DATA_DIR")
        .map(|(_, v)| v.clone());

    let watcher = tokio::spawn(async move {
        let status = child_for_watcher_wait(child).await;
        let detail = match &status {
            Ok(s) => format!("{s:?}"),
            Err(e) => format!("wait failed: {e}"),
        };
        tracing::warn!(
            shim_pid = child_pid,
            "shim subprocess exited unexpectedly: {detail}"
        );
        *exit_text_for_watcher.lock() = Some(detail.clone());
        crashed_for_watcher.store(true, std::sync::atomic::Ordering::SeqCst);
        // Drop all in-flight oneshots so callers in `send_and_await`
        // wake up immediately with RecvError → mapped to ShimFailure
        // by the caller.
        pending_for_watcher.clear();
        // Close the request channel so any further `send_and_await`
        // calls fail at the request_tx.send step rather than blocking.
        drop(request_tx_for_watcher);

        // Reap orphan Chromium subprocess tree. Chromium
        // and its helpers all carry --user-data-dir=<unique-per-session>
        // on their command line; pkill -f matches that pattern reliably.
        // No-op when LOOM_SHIM_USER_DATA_DIR wasn't set (test paths).
        if let Some(udd) = chromium_user_data_dir {
            let pattern = format!("user-data-dir={}", udd);
            tracing::info!("reaping orphan Chromium tree matching {pattern}");
            // SIGKILL because SIGTERM handlers are slow; orphan helpers
            // sometimes ignore SIGTERM when the parent is gone.
            let _ = std::process::Command::new("pkill")
                .arg("-9")
                .arg("-f")
                .arg(pattern)
                .status();
        }
    });

    Ok(Arc::new(ShimProcess {
        child: tokio::sync::Mutex::new(None), // moved into watcher
        child_pid,
        request_tx,
        pending,
        next_request_id: Arc::new(AtomicU64::new(1)),
        crashed,
        exit_status_text,
        tasks: ShimTasks {
            read,
            write,
            demux,
            watcher,
        },
    }))
}

/// Helper that takes ownership of the Child and awaits its exit.
/// Split out so the watcher closure can be `async move` cleanly.
async fn child_for_watcher_wait(
    mut child: tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait().await
}

async fn host_write_loop(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut request_rx: mpsc::Receiver<ShimRequest>,
) {
    use tokio::io::AsyncWriteExt;
    while let Some(req) = request_rx.recv().await {
        let mut payload = Vec::new();
        if let Err(e) = ciborium::ser::into_writer(&req, &mut payload) {
            tracing::error!("host: encode request: {e}");
            continue;
        }
        let len = payload.len() as u32;
        if write_half.write_all(&len.to_be_bytes()).await.is_err() {
            return;
        }
        if write_half.write_all(&payload).await.is_err() {
            return;
        }
    }
    let _ = write_half.shutdown().await;
}

/// Read loop for shim → host frames.
///
/// On a corrupt or oversized frame the transport is unrecoverable (the
/// stream is desynced — there is no way to find the next frame
/// boundary), so the loop marks the process crashed, records the reason,
/// and fails all pending requests — mirroring the child-exit watcher.
/// Without this, the process would stay cached as healthy: the next
/// `send_and_await` would pass the `crashed` fast-fail, write to the
/// half-dead socket, and park an unresolvable oneshot for the full
/// `recv_timeout` (30 s default; minutes for budget-extended navigates).
/// Once `crashed` is set, `get_or_spawn` evicts the entry on the next
/// call and respawns. Clean EOF / read errors are left to the watcher:
/// they accompany child exit, which the watcher detects and reports with
/// the real exit status.
///
/// `pub(crate)` as a test seam (mirrors `send_and_await`).
pub(crate) async fn host_read_loop(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    response_tx: mpsc::Sender<ShimResponse>,
    crashed: Arc<std::sync::atomic::AtomicBool>,
    exit_status_text: Arc<parking_lot::Mutex<Option<String>>>,
    pending: Arc<dashmap::DashMap<u64, oneshot::Sender<ShimResponse>>>,
) {
    use tokio::io::AsyncReadExt;
    loop {
        let mut len_buf = [0u8; 4];
        match read_half.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(_) => return,
        }
        let len = u32::from_be_bytes(len_buf);
        if len > loom_shared::shim_protocol::MAX_FRAME_BYTES {
            mark_transport_dead(
                format!("shim ipc framing failure: frame too large ({len} bytes)"),
                &crashed,
                &exit_status_text,
                &pending,
            );
            return;
        }
        let mut payload = vec![0u8; len as usize];
        if read_half.read_exact(&mut payload).await.is_err() {
            return;
        }
        let resp: ShimResponse = match ciborium::de::from_reader(&payload[..]) {
            Ok(r) => r,
            Err(e) => {
                mark_transport_dead(
                    format!("shim ipc framing failure: response decode: {e}"),
                    &crashed,
                    &exit_status_text,
                    &pending,
                );
                return;
            }
        };
        if response_tx.send(resp).await.is_err() {
            return;
        }
    }
}

/// Mark the shim transport dead from the read loop: record the reason,
/// flip `crashed` so `send_and_await` fail-fasts and `get_or_spawn`
/// evicts, and drop all in-flight oneshots so parked callers wake
/// immediately with RecvError → ShimFailure instead of stalling out the
/// full recv_timeout. Same sequence as the child-exit watcher.
fn mark_transport_dead(
    detail: String,
    crashed: &std::sync::atomic::AtomicBool,
    exit_status_text: &parking_lot::Mutex<Option<String>>,
    pending: &dashmap::DashMap<u64, oneshot::Sender<ShimResponse>>,
) {
    tracing::error!("host: {detail}");
    *exit_status_text.lock() = Some(detail);
    crashed.store(true, std::sync::atomic::Ordering::SeqCst);
    pending.clear();
}

/// `LoomError::context` key carrying which PHASE of a shim round-trip timed out,
/// and its two values. Both phases surface the same `shim_timeout` wire code, but
/// they are not the same failure:
///
/// - [`TIMEOUT_PHASE_SEND`] — the frame never left the host. The request channel
///   is wedged or the subprocess is gone: a transport failure.
/// - [`TIMEOUT_PHASE_RECV`] — the frame reached the shim and its answer did not
///   come back in time. That is a LIVE, responsive shim working on a slow page,
///   so it must NOT evict the subprocess and kill a healthy Chromium.
///
/// Carried as structured context rather than sniffed out of the message text, and
/// read back by `helpers::host_call_class`. Genuine shim death is detected
/// independently (the child-exit watcher and `mark_transport_dead` both set
/// `crashed`, which `register_and_send` fail-fasts on and `get_or_spawn` evicts),
/// so a recv timeout never has to stand in as a death signal.
pub(crate) const TIMEOUT_PHASE_KEY: &str = "timeout_phase";
pub(crate) const TIMEOUT_PHASE_SEND: &str = "send";
pub(crate) const TIMEOUT_PHASE_RECV: &str = "recv";

/// Register a pending-response slot and write `req` to the shim, returning the
/// `request_id` (for pending-cleanup on a later recv timeout) and the receiver
/// to await. An `Err` means the frame NEVER left the host (shim already
/// crashed, or the write failed/timed out) — the pending slot is cleaned up
/// before returning. Shared by [`send_and_await`] and
/// [`send_and_await_dispatch`] so both alloc the same unique, demux-routable
/// `request_id`.
async fn register_and_send(
    process: &ShimProcess,
    mut req: ShimRequest,
    send_timeout: Duration,
) -> Result<(u64, oneshot::Receiver<ShimResponse>), LoomError> {
    // Fast-fail if the watcher has already detected the shim's death.
    // Without this check we'd write to a dead socket, park a oneshot,
    // and time out at recv_timeout (~30s) — exactly the previously
    // observed hang.
    if process.crashed.load(std::sync::atomic::Ordering::SeqCst) {
        let detail = process
            .exit_status_text
            .lock()
            .clone()
            .unwrap_or_else(|| "shim subprocess exited (status unknown)".to_string());
        return Err(LoomError::new(
            LoomErrorCode::ShimFailure,
            format!("shim crashed: {detail}"),
        ));
    }

    let request_id = process.next_request_id.fetch_add(1, Ordering::SeqCst);
    set_request_id(&mut req, request_id);

    let (resp_tx, resp_rx) = oneshot::channel::<ShimResponse>();
    process.pending.insert(request_id, resp_tx);

    match tokio::time::timeout(send_timeout, process.request_tx.send(req)).await {
        Ok(Ok(())) => Ok((request_id, resp_rx)),
        Ok(Err(_)) => {
            process.pending.remove(&request_id);
            Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                "shim request channel closed".to_string(),
            ))
        }
        Err(_) => {
            process.pending.remove(&request_id);
            Err(LoomError::new(
                LoomErrorCode::ShimTimeout,
                format!("shim send timeout after {} ms", send_timeout.as_millis()),
            )
            .with_context(serde_json::json!({ TIMEOUT_PHASE_KEY: TIMEOUT_PHASE_SEND })))
        }
    }
}

/// Send a request and await the matching response with timeout.
pub async fn send_and_await(
    process: &ShimProcess,
    req: ShimRequest,
    send_timeout: Duration,
    recv_timeout: Duration,
) -> Result<ShimResponse, LoomError> {
    let (request_id, resp_rx) = register_and_send(process, req, send_timeout).await?;

    match tokio::time::timeout(recv_timeout, resp_rx).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(_)) => Err(LoomError::new(
            LoomErrorCode::ShimFailure,
            "shim response oneshot dropped (subprocess gone)".to_string(),
        )),
        Err(_) => {
            process.pending.remove(&request_id);
            Err(LoomError::new(
                LoomErrorCode::ShimTimeout,
                format!("shim recv timeout after {} ms", recv_timeout.as_millis()),
            )
            .with_context(serde_json::json!({ TIMEOUT_PHASE_KEY: TIMEOUT_PHASE_RECV })))
        }
    }
}

/// Outcome of a budget-bounded trusted-input dispatch send. Unlike
/// [`send_and_await`], a recv timeout is NOT an error: it means the frame was
/// written to the shim but its CDP ack never arrived within the budget — the
/// committing input most likely triggered a cross-origin renderer PROCESS SWAP
/// that tore the stale session down, so the ack lands on a renderer the host
/// isn't watching. The caller reports the input as performed-but-ack-pending.
/// A SEND failure/timeout (the frame never left the host) is still `Err`, so a
/// never-dispatched input is never mistaken for a performed one. Leak-safe: the
/// recv-timeout path removes its own `pending` entry (a late ack for the
/// abandoned id is dropped by the demux, `run_read_loop`).
pub enum DispatchAck {
    /// The shim answered within the budget.
    Response(ShimResponse),
    /// The frame was written but no ack returned within the budget.
    RecvTimeout,
}

/// Send a trusted-input frame and await its ack, but bound the wait by
/// `recv_budget` and report a recv timeout as [`DispatchAck::RecvTimeout`]
/// (input dispatched, ack lost) rather than a transport error. See [`DispatchAck`].
pub async fn send_and_await_dispatch(
    process: &ShimProcess,
    req: ShimRequest,
    send_timeout: Duration,
    recv_budget: Duration,
) -> Result<DispatchAck, LoomError> {
    let (request_id, resp_rx) = register_and_send(process, req, send_timeout).await?;

    match tokio::time::timeout(recv_budget, resp_rx).await {
        Ok(Ok(resp)) => Ok(DispatchAck::Response(resp)),
        Ok(Err(_)) => Err(LoomError::new(
            LoomErrorCode::ShimFailure,
            "shim response oneshot dropped (subprocess gone)".to_string(),
        )),
        Err(_) => {
            process.pending.remove(&request_id);
            Ok(DispatchAck::RecvTimeout)
        }
    }
}

fn set_request_id(req: &mut ShimRequest, id: u64) {
    match req {
        ShimRequest::SpawnTarget { request_id, .. }
        | ShimRequest::CdpSend { request_id, .. }
        | ShimRequest::PageNavigate { request_id, .. }
        | ShimRequest::WaitFor { request_id, .. }
        | ShimRequest::PageClose { request_id, .. }
        | ShimRequest::GetNetworkLog { request_id, .. }
        | ShimRequest::StartRecording { request_id, .. }
        | ShimRequest::StopRecording { request_id, .. }
        | ShimRequest::InjectAudio { request_id, .. }
        | ShimRequest::StartAudioCapture { request_id, .. }
        | ShimRequest::StopAudioCapture { request_id, .. }
        | ShimRequest::Shutdown { request_id }
        | ShimRequest::Health { request_id } => *request_id = id,
    }
}

/// Cooperatively shut a shim subprocess down: send Shutdown frame, await
/// the ack with a short deadline, then SIGTERM/SIGKILL the child.
pub async fn shutdown_process(process: Arc<ShimProcess>) {
    let _ = send_and_await(
        &process,
        ShimRequest::Shutdown { request_id: 0 },
        Duration::from_millis(500),
        Duration::from_secs(2),
    )
    .await;
    // The watcher task owns `Child`; the ShimProcess.child slot is None
    // post-spawn. Use libc::kill on the cached pid for SIGTERM, then
    // wait briefly for the watcher to flip `crashed` before escalating
    // to SIGKILL.
    let pid = process.child_pid;
    if !process.crashed.load(std::sync::atomic::Ordering::SeqCst) {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        // Wait up to 2s for the watcher to observe the exit.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if process.crashed.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Still alive — escalate.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        // Final brief wait so the watcher's exit_status_text is set
        // before any caller reads it.
        let _ = tokio::time::timeout(Duration::from_secs(1), wait_for_crashed_flag(&process)).await;
    }
}

async fn wait_for_crashed_flag(process: &ShimProcess) {
    while !process.crashed.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
