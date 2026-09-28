//! `fake-chromium` — test-only binary that simulates a real Chromium
//! Chrome DevTools Protocol endpoint for integration tests.
//!
//! Behaviour:
//!   1. Bind a tokio-tungstenite WebSocket server on 127.0.0.1:0.
//!   2. Wait for the kernel-assigned port to be ready.
//!   3. Print `"DevTools listening on ws://127.0.0.1:<port>/..."` to
//!      stderr (matching real Chromium's startup line).
//!   4. Also write `<port>\n<path>` to `<user_data_dir>/DevToolsActivePort`
//!      if `LOOM_FAKE_CHROMIUM_USER_DATA_DIR` is set (matches Chromium's
//!      file-based discovery path).
//!   5. Accept WebSocket connections and respond to canned CDP methods.
//!   6. Exit cleanly on SIGTERM / Ctrl-C.
//!
//! Optional env vars:
//! - `LOOM_FAKE_CHROMIUM_USER_DATA_DIR` — write `DevToolsActivePort` here.
//! - `LOOM_FAKE_CHROMIUM_LOG` — append a JSON line per received method
//!   (used by integration tests to assert what the daemon sent).
//! - `LOOM_FAKE_CHROMIUM_FAIL_AFTER_N` — close WS after N requests
//!   (used to test surface_unavailable).
//! - `LOOM_FAKE_CHROMIUM_FIXTURE` — path to a JSON file containing a
//!   tiny DOM model used to answer `DOM.querySelector`, `DOM.getBoxModel`,
//!   `DOM.scrollIntoViewIfNeeded`, and `Page.getLayoutMetrics`. Shape:
//!   `{ "boxes": { "<selector>": [x1, y1, x2, y2] }, "viewport": [w, h] }`.
//!   Used by the Click/Hover/Scroll hit-test integration tests. Optional
//!   `"inputs": { "<selector>": "<fill verdict>" }` scripts `web.type` fill's
//!   prepare step for a selector that also has a box: `DOM.resolveNode` hands
//!   out a `fake-node:<nodeId>` object and node-scoped `Runtime.callFunctionOn`
//!   answers the verdict — `set`, `insert` (the default), `not_editable`,
//!   `detached`, `malformed:<input type>`, `throw` (a page exception), `garbage`
//!   (an unknown verdict), `no_object` / `resolve_error` (`DOM.resolveNode`
//!   returns no objectId / a CDP error), or `swallow_resolve` / `swallow_call`
//!   (that message is never answered — a lost ack). `"release_error": true` makes
//!   `Runtime.releaseObjectGroup` fail; `"slow_focus_ms": N` answers `DOM.focus`
//!   N ms late (a budget spent during selector resolution, on any host).
//! - `LOOM_FAKE_CHROMIUM_SCRIPT` — path to a JSON file driving the
//!   settle-capture readiness probe deterministically across ticks. Shape:
//!   `{ "settle_probe": [[ready_complete, "href", dom_mutations], ...],
//!      "perpetual_inflight": N }`.
//!   The i-th settle probe `Runtime.evaluate` (the one carrying
//!   `__loomSettleMut`) returns `settle_probe[i]` (the last entry repeats
//!   once exhausted), letting a test script a client-side redirect
//!   (href changes then stabilises), async-after-load content (a late
//!   DOM-mutation burst), or a never-settling DOM (perpetual mutations).
//!   `perpetual_inflight` pins N never-finishing in-flight requests
//!   (re-asserted on every probe so the wait sees them regardless of when
//!   the host's Network handler registered) → drives the bounded-timeout
//!   path. The settle-capture never-settles / redirect e2e cases use this.

mod audio;
mod canned;
mod conn;
mod dom_fixture;
mod evaluate;
mod faults;
mod fetch_gate;
mod navigate_events;
mod screencast;
mod settle_probe;
mod settle_script;
mod url_pattern;
mod virtual_time;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::conn::Conn;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // STEP 1: bind first.
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fake-chromium: bind failed: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let addr = listener.local_addr().unwrap();
    let ws_path = "/devtools/browser/fake-chromium-test";

    // STEP 4: write DevToolsActivePort file (real Chromium does this).
    if let Ok(udd) = std::env::var("LOOM_FAKE_CHROMIUM_USER_DATA_DIR") {
        let path = PathBuf::from(&udd).join("DevToolsActivePort");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, format!("{}\n{}", addr.port(), ws_path));
    }

    // STEP 3: print the canonical startup line.
    eprintln!(
        "DevTools listening on ws://127.0.0.1:{}{}",
        addr.port(),
        ws_path
    );
    let _ = std::io::stderr().flush();

    let log_path: Option<PathBuf> = std::env::var("LOOM_FAKE_CHROMIUM_LOG")
        .ok()
        .map(PathBuf::from);
    let fail_after: Option<usize> = std::env::var("LOOM_FAKE_CHROMIUM_FAIL_AFTER_N")
        .ok()
        .and_then(|s| s.parse().ok());

    // STEP 5+6: accept loop, racing against shutdown signal.
    tokio::select! {
        _ = accept_loop(listener, log_path, fail_after) => {
            // accept_loop only returns on listener error
            std::process::ExitCode::SUCCESS
        }
        _ = tokio::signal::ctrl_c() => {
            std::process::ExitCode::SUCCESS
        }
    }
}

async fn accept_loop(listener: TcpListener, log_path: Option<PathBuf>, fail_after: Option<usize>) {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("fake-chromium: accept failed: {e}");
                return;
            }
        };
        let log = log_path.clone();
        tokio::spawn(async move {
            handle_connection(stream, log, fail_after).await;
        });
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    log_path: Option<PathBuf>,
    fail_after: Option<usize>,
) {
    let require_session_id =
        std::env::var("LOOM_FAKE_CHROMIUM_REQUIRE_SESSION_ID").as_deref() == Ok("1");

    let ws = match accept_async(stream).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fake-chromium: WS handshake failed: {e}");
            return;
        }
    };
    let (write, mut read) = ws.split();
    let mut conn = Conn::new(write, require_session_id);
    let mut request_count = 0usize;

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("fake-chromium: WS read error: {e}");
                return;
            }
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return,
            Message::Ping(p) => {
                let _ = conn.write.send(Message::Pong(p)).await;
                continue;
            }
            _ => continue,
        };

        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fake-chromium: JSON decode: {e}");
                continue;
            }
        };

        // Append to log file if requested.
        if let Some(p) = &log_path {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                let _ = writeln!(f, "{}", value);
            }
        }

        request_count += 1;

        // Optional crash injection.
        if let Some(n) = fail_after {
            if request_count > n {
                eprintln!("fake-chromium: closing WS after {n} requests");
                let _ = conn.write.send(Message::Close(None)).await;
                return;
            }
        }

        let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let method = value
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let session_id = value
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from);

        if conn.serve(id, &method, &params, &session_id).await.is_err() {
            return;
        }
    }
}
