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
//!   `Runtime.releaseObjectGroup` fail.
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

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

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

/// A minimal valid 1×1 JPEG, base64-encoded — emitted as a synthetic screencast
/// frame (video-capture e2e). Valid JPEG bytes so a real-ffmpeg encode in the
/// `#[ignore]`d e2e succeeds; the shim recorder only base64-decodes + buffers it.
const TINY_JPEG_BASE64: &str = "/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjI4LjEwMQD/2wBDAAgEBAQEBAUFBQUFBQYGBgYGBgYGBgYGBgYHBwcICAgHBwcGBgcHCAgICAkJCQgICAgJCQoKCgwMCwsODg4RERT/xABLAAEBAAAAAAAAAAAAAAAAAAAACAEBAAAAAAAAAAAAAAAAAAAAABABAAAAAAAAAAAAAAAAAAAAABEBAAAAAAAAAAAAAAAAAAAAAP/AABEIAAIAAgMBIgACEQADEQD/2gAMAwEAAhEDEQA/AJ/AB//Z";

async fn handle_connection(
    stream: tokio::net::TcpStream,
    log_path: Option<PathBuf>,
    fail_after: Option<usize>,
) {
    // When LOOM_FAKE_CHROMIUM_REQUIRE_SESSION_ID=1, validate
    // that every page-scope CDP request carries a top-level `sessionId`
    // field. Real Chromium rejects page-scope methods without sessionId
    // with `{code:-32601, message:"'<method>' wasn't found"}`; this flag
    // makes the integration test enforce the same constraint.
    let require_session_id =
        std::env::var("LOOM_FAKE_CHROMIUM_REQUIRE_SESSION_ID").as_deref() == Ok("1");

    let ws = match accept_async(stream).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fake-chromium: WS handshake failed: {e}");
            return;
        }
    };
    let (mut write, mut read) = ws.split();
    let mut request_count = 0usize;
    // Real Chromium only delivers `Fetch.requestPaused` events AFTER the
    // host issues `Fetch.enable`. Mirror that here so the synthetic
    // PageWithTracker emission below doesn't fire when the host has
    // disabled the blocklist gate (`blocklist_enabled = false` →
    // `subscribe()` skipped → no `Fetch.enable`).
    let mut fetch_enabled = false;
    // settle-capture: per-connection cursor into LOOM_FAKE_CHROMIUM_SCRIPT's
    // `settle_probe` array. Reset on each Page.navigate so every navigation
    // replays the script from the top; advanced once per settle probe.
    let script = settle_script();
    let mut settle_idx: usize = 0;
    // Virtual-time fidelity: real headless Chromium DEFERS the load-completion
    // tasks while virtual time is not advancing — after the inject-time
    // `setVirtualTimePolicy {policy:"pause"}` pin, `Page.loadEventFired` is
    // held until a budget-carrying setVirtualTimePolicy advances the clock
    // (and the clock pauses again once each budget drains). Mirror that:
    // once a pause-pin has been seen, every navigate's loadEventFired is
    // deferred until the next budget arm, which flushes it just before the
    // synthetic `virtualTimeBudgetExpired`. An executor that awaits load
    // BEFORE arming the budget deadlocks here exactly like it does against
    // real Chromium (the settle-timeout-on-static regression).
    let mut vt_clock_paused = false;
    let mut deferred_load_event: Option<String> = None;
    // Fetch-pause fidelity: with `Fetch.enable {requestStage:"Request"}`,
    // real Chromium pauses the matched DOCUMENT request and the navigation
    // does NOT proceed — no Network.responseReceived, no Page.loadEventFired
    // — until the client answers Fetch.continueRequest / failRequest for the
    // paused requestId. Mirror that: the PageWithTracker document's would-be
    // events are stashed here and only flushed when the answer arrives, so
    // an interceptor regression that never answers the document pause hangs
    // the navigate in e2e exactly like it would against real Chromium
    // (previously the fake emitted them fire-and-forget and could not catch
    // that divergence class).
    let mut paused_doc: Option<PausedDoc> = None;
    // Under `pauseIfNetworkFetchesPending`, a paused document fetch keeps
    // virtual time from advancing: a budget armed while the pause is
    // outstanding must not drain (no flush, no virtualTimeBudgetExpired)
    // until the fetch gate answers.
    let mut vt_budget_pending_on_pause = false;
    // Interaction-fingerprint (capture-policy=fingerprint) e2e hook: a
    // `__loom_test_dom_mutate__` Runtime.evaluate (modeling a DOM-mutating click)
    // flips this per-connection flag so a SUBSEQUENT DOM.getDocument returns
    // content-differing DOM. Lets the e2e prove `dom_after_hash` is content-bearing
    // (differs from a no-op) yet deterministic across same-seed sessions (the
    // ephemeral frameIds still vary per call and must be normalized away).
    let mut dom_after_mutated = false;
    // Client-side-redirect modeling (see `SettleScript::renavigate_at`): true
    // while the loaded page has begun a self-initiated top-level navigation
    // whose new document is held `readyState:"loading"` until the executor
    // re-arms the virtual-time budget. `renav_href` is the URL the wedged
    // in-flight document reports until then.
    let mut awaiting_rearm = false;
    let mut renav_href = String::new();
    // Cross-origin process-swap gate: set when the current `awaiting_rearm`
    // wedge was triggered by `cross_origin_swap_at` (not `renavigate_at`). While
    // true, the re-arm handler SUPPRESSES the deferred load-flush + budget-expiry
    // (the swapped-away renderer's events never reach this stale session), so a
    // buggy multi-phase executor dead-waits each phase and a correct one returns
    // a bounded typed `timeout`.
    let mut cross_origin_swap_active = false;
    // voice-call-io task 07: per-connection echo buffer for the audio harness
    // (`LOOM_FAKE_CHROMIUM_AUDIO_ECHO`). Set from the `enqueue` argument, taken on the
    // first `drain`. See `audio_call_function_on`.
    let mut audio_echo_b64: Option<String> = None;

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
                let _ = write.send(Message::Pong(p)).await;
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
                let _ = write.send(Message::Close(None)).await;
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

        // Enforcement: page-scope methods MUST carry sessionId.
        if require_session_id && session_id.is_none() && !is_browser_scope_method_local(&method) {
            let err = json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("'{method}' wasn't found"),
                },
            });
            let _ = write.send(Message::Text(err.to_string().into())).await;
            continue;
        }

        // Cross-origin process-swap of the trusted-click DISPATCH: withhold the
        // ack for the commit event (`Input.dispatchMouseEvent type:"mousePressed"`)
        // so the stale CDP session never observes it — the renderer swapped away.
        // The host must bound the dispatch by its budget and return a typed
        // "dispatched" outcome, never dead-wait the full recv timeout. `mouseMoved`
        // / `mouseReleased` are still acked so ONLY the commit event's ack is lost.
        if settle_script().swallow_dispatch_ack
            && method == "Input.dispatchMouseEvent"
            && params.get("type").and_then(|t| t.as_str()) == Some("mousePressed")
        {
            continue;
        }

        // Cross-origin swap whose window opens at/before the move: the stale CDP
        // session goes blind before even the PRE-COMMIT (`mouseMoved`) ack, so
        // withhold the ack for EVERY trusted-click mouse frame. This exercises the
        // surviving "input dispatch ack timeout before commit" arm that
        // `swallow_dispatch_ack` (commit-frame only) cannot reach.
        if settle_script().swallow_dispatch_ack_from_move && method == "Input.dispatchMouseEvent" {
            continue;
        }

        // web.type fill prepare step losing its ack (fixture `inputs` verdicts
        // `swallow_resolve` / `swallow_call`): the host must bound the wait by its
        // budget and report an error — never a success, never the 30 s recv floor.
        if fill_prepare_ack_swallowed(&method, &params) {
            continue;
        }

        // Withhold ONLY the PRE-COMMIT (`mouseMoved`) ack; the committing frames
        // still ack. Models a transient move-ack drop on a LIVE session — the click
        // still commits cleanly, so the host must return `Ok` (a dropped move ack
        // alone must NOT degrade a fully-committed click to `DispatchedAckPending`).
        if settle_script().swallow_dispatch_ack_move_only
            && method == "Input.dispatchMouseEvent"
            && params.get("type").and_then(|t| t.as_str()) == Some("mouseMoved")
        {
            continue;
        }

        // Genuine CDP application error on the committing frame (renderer rejects
        // the input, e.g. a detached/invalid node) — distinct from a swap ack loss.
        // The host MUST surface this as a hard failure (the `Some(Err)` arm of
        // `dispatch_input_events`), never mask it as a performed click.
        if settle_script().dispatch_cdp_error
            && method == "Input.dispatchMouseEvent"
            && params.get("type").and_then(|t| t.as_str()) == Some("mousePressed")
        {
            let mut err = json!({
                "id": id,
                "error": { "code": -32000, "message": "Could not dispatch mouse event" },
            });
            if let Some(sid) = &session_id {
                err["sessionId"] = json!(sid);
            }
            let _ = write.send(Message::Text(err.to_string().into())).await;
            continue;
        }

        // Model real Chrome's browser-process ack latency for the trusted-click
        // commit event: delay (but still WRITE) the `mousePressed` ack. A correct
        // dispatch budget clears this latency → the ack arrives → `Ok`; a budget
        // collapsed below it would spuriously time out. Falls through to the
        // normal response path after the delay.
        if settle_script().dispatch_ack_delay_ms > 0
            && method == "Input.dispatchMouseEvent"
            && params.get("type").and_then(|t| t.as_str()) == Some("mousePressed")
        {
            tokio::time::sleep(std::time::Duration::from_millis(
                settle_script().dispatch_ack_delay_ms,
            ))
            .await;
        }

        // For Page.navigate, derive a per-URL response shape so integration
        // tests can drive the receipt across success / 4xx / 5xx /
        // transport-error branches without touching real Chromium.
        // Conventions:
        //   http://fake.test/status/<N>  → emit Network.responseReceived
        //                                   with type=Document, status=N
        //   http://fake.test/error/<CDP> → set errorText=<CDP> in the
        //                                   Page.navigate response AND emit
        //                                   Network.loadingFailed with
        //                                   type=Document, errorText=<CDP>
        //   http://fake.test/page-with-iframe-404
        //                                → main Document 200 + IFRAME
        //                                   Document 404 (distinct frame/
        //                                   loader ids) — navigate must
        //                                   still succeed
        //   anything else                → bare canned response (legacy)
        let nav_url_pattern = if method == "Page.navigate" {
            params
                .get("url")
                .and_then(|v| v.as_str())
                .map(parse_fake_url_pattern)
                .unwrap_or(FakeUrlPattern::None)
        } else {
            FakeUrlPattern::None
        };

        // Page.navigate resets the settle-script cursor so each navigation
        // replays LOOM_FAKE_CHROMIUM_SCRIPT from the top. Any document
        // pause left over from a prior navigate is superseded too.
        if method == "Page.navigate" {
            settle_idx = 0;
            paused_doc = None;
            vt_budget_pending_on_pause = false;
            awaiting_rearm = false;
            renav_href = String::new();
        }

        // Runtime.evaluate is driven by an expression-pattern convention
        // (parallels Page.navigate's URL-pattern scheme above) so
        // integration tests can drive every evaluate-result branch
        // synthetically. See `parse_fake_evaluate_pattern` for the
        // sentinel grammar. The settle-capture readiness probe (carrying the
        // `__loomSettleMut` global) is special-cased here because its
        // response advances per-connection script state.
        let expression = if method == "Runtime.evaluate" {
            params
                .get("expression")
                .and_then(|v| v.as_str())
                .map(String::from)
        } else {
            None
        };
        let is_settle_probe = expression
            .as_deref()
            .map(|e| e.contains("__loomSettleMut"))
            .unwrap_or(false);
        let evaluate_response = if let Some(expr) = &expression {
            if is_settle_probe {
                // Client-side-redirect gate: if the page is scripted to begin a
                // self-initiated top-level navigation at this tick, queue a NEW
                // load event held on the next budget arm and pin the probe to
                // "loading" until the executor re-arms (see `renavigate_at`).
                // A `cross_origin_swap_at` tick enters the SAME wedge but marks
                // `cross_origin_swap_active` so the re-arm handler suppresses the
                // flush + expiry (the swapped renderer's events never reach this
                // stale session).
                let swap_tick = script.cross_origin_swap_at.contains(&settle_idx);
                if !awaiting_rearm && (script.renavigate_at.contains(&settle_idx) || swap_tick) {
                    awaiting_rearm = true;
                    cross_origin_swap_active = swap_tick;
                    renav_href = script
                        .probe
                        .get(settle_idx)
                        .map(|(_, h, _)| h.clone())
                        .unwrap_or_default();
                    let mut evt = json!({
                        "method": "Page.loadEventFired",
                        "params": { "timestamp": 1.0 }
                    });
                    if let Some(sid) = &session_id {
                        evt["sessionId"] = json!(sid);
                    }
                    // The new document's load is held until the clock advances,
                    // exactly like the shell's was. The clock is already paused
                    // again (it re-pauses after each budget drains). For a
                    // cross-origin swap this held event is NEVER flushed (see the
                    // re-arm handler) — it models the load landing on a renderer
                    // the stale session cannot see.
                    deferred_load_event = Some(evt.to_string());
                    vt_clock_paused = true;
                }
                if awaiting_rearm {
                    if cross_origin_swap_active {
                        // Cross-origin swap: the OLD execution context is
                        // destroyed, so `Runtime.evaluate` for the settle probe
                        // fails. Real Chromium returns a "Cannot find context
                        // with specified id" error; the shim's `probe_page` maps
                        // any CDP error to "not settled". Use the `__cdp_error__`
                        // sentinel so the response path emits a JSON-RPC error
                        // envelope (not a `result`).
                        Some(json!({ "__cdp_error__": {
                            "code": -32000,
                            "message": "Cannot find context with specified id"
                        }}))
                    } else {
                        // Same-process wedge: readyState stays "loading" until a
                        // re-arm flushes the held load event.
                        let encoded = json!([false, renav_href, 0]).to_string();
                        Some(json!({ "result": { "type": "string", "value": encoded } }))
                    }
                } else {
                    let resp = script.probe_response(settle_idx);
                    settle_idx += 1;
                    Some(resp)
                }
            } else {
                Some(build_fake_evaluate_response(expr))
            }
        } else {
            None
        };

        // Track Fetch domain enable state so the PageWithTracker branch
        // below can mirror real Chromium's "events only after Fetch.enable"
        // semantics.
        if method == "Fetch.enable" {
            fetch_enabled = true;
        } else if method == "Fetch.disable" {
            fetch_enabled = false;
        }

        // Track the virtual-time clock pin (see `vt_clock_paused` above). A
        // budgetless `policy:"pause"` is the inject-time origin pin; the
        // budget-carrying arm is handled after the response is sent below.
        if method == "Emulation.setVirtualTimePolicy" && params.get("budget").is_none() {
            match params.get("policy").and_then(|p| p.as_str()) {
                // Inject-time origin pin (determinism ON) — clock paused.
                Some("pause") => vt_clock_paused = true,
                // Navigate-exit RESUME (`resume_virtual_time`,
                // `build_virtual_time_resume_params` → `{policy:"advance"}`, no
                // budget): real Chromium lets virtual time advance freely again, so
                // the clock un-pauses and the NEXT navigate's load fires promptly.
                // Modeling this is what makes the navigate-degradation regression
                // observable here: WITHOUT the resume-guard fix the executor never
                // sends this advance after a clean drain under `--no-determinism`,
                // so `vt_clock_paused` stays true (set when the budget drained
                // below) and the next navigate's load defers → the +20s/call wedge.
                Some("advance") => vt_clock_paused = false,
                _ => {}
            }
        }

        // voice-call-io task 07: audio harness answers the four nonce'd in-page audio
        // API calls (enqueue/startCapture/stopCapture/drain). Handled here (not in the
        // stateless `canned_response`) because echo mode threads per-connection state.
        let audio_response = if method == "Runtime.callFunctionOn" {
            audio_call_function_on(&params, &mut audio_echo_b64)
        } else {
            None
        };

        let mut result = if let Some(eval_result) = evaluate_response {
            eval_result
        } else if let Some(audio_result) = audio_response {
            audio_result
        } else {
            canned_response(&method, &params)
        };
        // Interaction-fingerprint (capture-policy=fingerprint) e2e hook: a
        // `__loom_test_dom_mutate__` Runtime.evaluate (modeling a DOM-mutating
        // click) flips the per-connection flag; a SUBSEQUENT DOM.getDocument then
        // returns content-differing DOM (a content text node, NOT an ephemeral id,
        // so normalization keeps it → the dom_after_hash changes vs a no-op).
        if let Some(expr) = &expression {
            if expr.contains("__loom_test_dom_mutate__") {
                dom_after_mutated = true;
            }
        }
        if method == "DOM.getDocument" && dom_after_mutated {
            if let Some(children) = result
                .pointer_mut("/root/children")
                .and_then(|c| c.as_array_mut())
            {
                children.push(json!({
                    "nodeId": 9001,
                    "backendNodeId": 9001,
                    "nodeName": "#text",
                    "nodeType": 3,
                    "nodeValue": "loom-dom-after-mutated"
                }));
            }
        }
        if method == "Page.navigate" {
            if let FakeUrlPattern::Error(ref code) = nav_url_pattern {
                result["errorText"] = json!(code);
            }
            // `http://fake.test/slow/<MS>`: stall this navigate's response so a
            // shim per-CDP-command navigate-budget timeout fires. Drives the
            // `LOOM_SHIM_CDP_TIMEOUT_MS` e2e (both the raised-budget success and
            // the default-budget typed-timeout cases).
            if let FakeUrlPattern::Slow(ms) = nav_url_pattern {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }

        // Stale-event injection sentinel for Runtime.evaluate
        // (`__loom_test_emit_doc_event__:<status>`): emit a Document
        // requestWillBeSent + responseReceived BEFORE the evaluate
        // response, modeling an in-session CLICK that triggered a real
        // link navigation between two navigates. Those Document events
        // accumulate in the shim's hashed path with NO drain until the
        // next navigate — which must discard them at its START
        // (`clear_events`) instead of letting them poison its
        // status_code / network_events. Distinct loaderId so loader
        // matching can also tell it apart from a current navigation.
        if let Some(expr) = &expression {
            if let Some(rest) = expr.strip_prefix("__loom_test_emit_doc_event__:") {
                if let Ok(status) = rest.parse::<u16>() {
                    let click_url = "http://fake.test/clicked-link";
                    let mut click_req = json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "fake-req-click-1",
                            "frameId": "fake-frame-1",
                            "loaderId": "fake-loader-click-1",
                            "timestamp": 2.0,
                            "wallTime": 1_700_000_002.0,
                            "type": "Document",
                            "request": { "url": click_url, "method": "GET" },
                        },
                    });
                    if let Some(sid) = &session_id {
                        click_req["sessionId"] = json!(sid);
                    }
                    let _ = write
                        .send(Message::Text(click_req.to_string().into()))
                        .await;

                    let mut click_resp = json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "fake-req-click-1",
                            "frameId": "fake-frame-1",
                            "loaderId": "fake-loader-click-1",
                            "timestamp": 2.1,
                            "type": "Document",
                            "response": {
                                "url": click_url,
                                "status": status,
                                "statusText": "",
                                "mimeType": "text/html",
                            },
                        },
                    });
                    if let Some(sid) = &session_id {
                        click_resp["sessionId"] = json!(sid);
                    }
                    let _ = write
                        .send(Message::Text(click_resp.to_string().into()))
                        .await;
                }
            }
        }

        // CDP error sentinel: `canned_response` may return
        // `{"__cdp_error__": {"code": ..., "message": ...}}` to signal
        // that this method should respond with a JSON-RPC error envelope
        // instead of `{result: ...}`. Used by `DOM.getBoxModel` on
        // hidden / zero-area / unknown nodes.
        let mut response = if let Some(err) = result.get("__cdp_error__").cloned() {
            json!({ "id": id, "error": err })
        } else {
            json!({ "id": id, "result": result })
        };
        if let Some(sid) = &session_id {
            response["sessionId"] = json!(sid);
        }
        let response_text = response.to_string();
        if write
            .send(Message::Text(response_text.into()))
            .await
            .is_err()
        {
            return;
        }

        // voice-call-io task 07: audio harness env hooks (mirroring
        // LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES), consumed in `audio_call_function_on`:
        //   LOOM_FAKE_CHROMIUM_AUDIO_ECHO=1        — replay injected bytes out of drain
        //                                            (the AC4 fake peer, 16 kHz passthrough).
        //   LOOM_FAKE_CHROMIUM_AUDIO_TONE=<f>:<ms> — emit a canned 48 kHz sine (AC3-shape).
        //   LOOM_FAKE_CHROMIUM_AUDIO_NO_GUM=1      — enqueue rejects no_microphone_request (AC10).

        // video-capture: on Page.startScreencast, emit N synthetic
        // Page.screencastFrame events (each a tiny valid JPEG) so the shim's
        // ScreencastRecorder can be exercised end-to-end without real Chromium.
        // N comes from LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES (default 0 = none).
        // The shim acks each frame via Page.screencastFrameAck (default {} Ok).
        if method == "Page.startScreencast" {
            let n: u32 = std::env::var("LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            for frame_no in 1..=n {
                let mut frame = json!({
                    "method": "Page.screencastFrame",
                    "params": {
                        "data": TINY_JPEG_BASE64,
                        "metadata": {
                            "offsetTop": 0,
                            "pageScaleFactor": 1,
                            "deviceWidth": 1,
                            "deviceHeight": 1,
                            "scrollOffsetX": 0,
                            "scrollOffsetY": 0,
                            "timestamp": frame_no as f64,
                        },
                        "sessionId": frame_no,
                    },
                });
                if let Some(sid) = &session_id {
                    frame["sessionId"] = json!(sid);
                }
                if write
                    .send(Message::Text(frame.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        // A budget-carrying setVirtualTimePolicy advances the clock: first
        // flush any load event deferred by the pause-pin (real Chromium
        // completes the held load tasks the moment the budget is granted),
        // then emit Emulation.virtualTimeBudgetExpired so the action_executor's
        // budget await (cross-run determinism) completes promptly instead of
        // waiting out its wall-clock timeout. The budget is treated as
        // instantly drained (the fake has no real virtual clock), after which
        // the clock is paused again so the NEXT navigate's load event defers
        // until ITS budget arm, exactly like a second navigation against real
        // Chromium (CDP `virtualTimeBudgetExpired` leaves virtual time paused).
        // This re-pause is what reproduces the navigate-degradation wedge under
        // `--no-determinism`: the executor must send a budgetless `advance`
        // (resume) on navigate exit to un-pause before the next navigate, which
        // the resume-guard fix does (the unfixed `!budget_drained` guard skipped
        // it after a clean drain, so the clock stayed paused and the next load
        // deferred for the full budget).
        if method == "Emulation.setVirtualTimePolicy" && params.get("budget").is_some() {
            if paused_doc.is_some() {
                // `pauseIfNetworkFetchesPending`: the paused document fetch
                // keeps virtual time from advancing, so the budget cannot
                // drain (and the held load cannot flush) until the Fetch
                // gate answers. The continueRequest/failRequest handler
                // below emits the budget expiry once the pause resolves.
                vt_budget_pending_on_pause = true;
            } else if cross_origin_swap_active {
                // Cross-origin process swap: the destination's load event and
                // the virtual-time budget expiry both land on a NEW renderer
                // this stale session cannot observe. Suppress BOTH — do NOT
                // flush `deferred_load_event`, do NOT emit
                // `virtualTimeBudgetExpired`. The re-arm command still gets its
                // ack (sent by the generic response path above), so the
                // executor's arm succeeds but then dead-waits the event that
                // never comes — bounded only by its own deadline. The wedge is
                // never cleared (never-recovering).
                vt_clock_paused = true;
            } else {
                if let Some(load_evt) = deferred_load_event.take() {
                    if write.send(Message::Text(load_evt.into())).await.is_err() {
                        return;
                    }
                    // A re-arm that flushed a renavigation's held load event
                    // un-wedges the new document: clear the loading gate and
                    // step past the renav tick so the next probe returns the
                    // scripted post-redirect (complete) observation.
                    if awaiting_rearm {
                        awaiting_rearm = false;
                        settle_idx += 1;
                    }
                }
                let mut vt_evt = json!({
                    "method": "Emulation.virtualTimeBudgetExpired",
                    "params": {},
                });
                if let Some(sid) = &session_id {
                    vt_evt["sessionId"] = json!(sid);
                }
                if write
                    .send(Message::Text(vt_evt.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                // Real Chromium leaves virtual time PAUSED once a budget drains
                // (until the next policy is set). Model that so a subsequent
                // navigate's load event defers — the executor must explicitly
                // resume (`advance`) to clear it.
                vt_clock_paused = true;
            }
        }

        // Answering the document's Fetch pause releases the held
        // navigation: continueRequest flushes the document response +
        // load event (real Chromium resumes the request); failRequest
        // aborts it with a Document loadingFailed and NO load event.
        if method == "Fetch.continueRequest" || method == "Fetch.failRequest" {
            let answered_id = params
                .get("requestId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if paused_doc
                .as_ref()
                .is_some_and(|pd| pd.request_id == answered_id)
            {
                let pd = paused_doc.take().expect("checked is_some above");
                if method == "Fetch.continueRequest" {
                    for held in pd.held_events {
                        if write.send(Message::Text(held.into())).await.is_err() {
                            return;
                        }
                    }
                    if let Some(load_evt) = pd.load_event {
                        if vt_budget_pending_on_pause {
                            // The armed budget resumes draining now that
                            // the fetch gate is clear: the held load
                            // completes, then the budget expires.
                            vt_budget_pending_on_pause = false;
                            if write.send(Message::Text(load_evt.into())).await.is_err() {
                                return;
                            }
                            let mut vt_evt = json!({
                                "method": "Emulation.virtualTimeBudgetExpired",
                                "params": {},
                            });
                            if let Some(sid) = &session_id {
                                vt_evt["sessionId"] = json!(sid);
                            }
                            if write
                                .send(Message::Text(vt_evt.to_string().into()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        } else if vt_clock_paused {
                            deferred_load_event = Some(load_evt);
                        } else if write.send(Message::Text(load_evt.into())).await.is_err() {
                            return;
                        }
                    }
                } else {
                    // Document failRequest: navigation aborted — emit the
                    // Document loadingFailed real Chromium produces for a
                    // client-blocked main document; the held events drop.
                    let mut fail_evt = json!({
                        "method": "Network.loadingFailed",
                        "params": {
                            "requestId": "fake-req-1",
                            "timestamp": 1.0,
                            "type": "Document",
                            "errorText": "net::ERR_BLOCKED_BY_CLIENT",
                            "canceled": false,
                        },
                    });
                    if let Some(sid) = &session_id {
                        fail_evt["sessionId"] = json!(sid);
                    }
                    if write
                        .send(Message::Text(fail_evt.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if vt_budget_pending_on_pause {
                        // The aborted fetch no longer pends; the budget
                        // drains with nothing further to flush.
                        vt_budget_pending_on_pause = false;
                        let mut vt_evt = json!({
                            "method": "Emulation.virtualTimeBudgetExpired",
                            "params": {},
                        });
                        if let Some(sid) = &session_id {
                            vt_evt["sessionId"] = json!(sid);
                        }
                        if write
                            .send(Message::Text(vt_evt.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }

        // settle-capture never-settles (network) shape: re-assert N
        // never-finishing in-flight requests on every settle probe. Stable
        // requestIds make the inserts idempotent in the host's in-flight set,
        // so the count stays pinned at N regardless of when the host's
        // `Network.` handler registered (it registers only once the settle
        // wait begins, after this navigate's load fires). N > the idle
        // threshold keeps `networkidle` from ever quiescing → bounded Timeout.
        if is_settle_probe && script.perpetual_inflight > 0 {
            for i in 0..script.perpetual_inflight {
                let mut evt = json!({
                    "method": "Network.requestWillBeSent",
                    "params": {
                        "requestId": format!("perpetual-{i}"),
                        "type": "Fetch",
                        "request": { "url": "http://fake.test/poll", "method": "GET" },
                    },
                });
                if let Some(sid) = &session_id {
                    evt["sessionId"] = json!(sid);
                }
                if write
                    .send(Message::Text(evt.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        // Per-URL synthetic CDP events emitted right after Page.navigate
        // resolves. Order matches what real Chromium produces:
        // Network.* events fire before Page.loadEventFired.
        if method == "Page.navigate" {
            let nav_url = params
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match &nav_url_pattern {
                FakeUrlPattern::Status(status) => {
                    // Document requestWillBeSent FIRST (real Chromium order) so
                    // the full-capture accumulator records the HTTP method —
                    // responseReceived alone has no method.
                    let mut doc_req = json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "fake-req-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.0,
                            "wallTime": 1_700_000_000.0,
                            "type": "Document",
                            "request": { "url": nav_url, "method": "GET" },
                        },
                    });
                    if let Some(sid) = &session_id {
                        doc_req["sessionId"] = json!(sid);
                    }
                    let _ = write.send(Message::Text(doc_req.to_string().into())).await;

                    let mut evt = json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "fake-req-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.0,
                            "type": "Document",
                            "response": {
                                "url": nav_url,
                                "status": status,
                                "statusText": "",
                                "mimeType": "text/html",
                            },
                        },
                    });
                    if let Some(sid) = &session_id {
                        evt["sessionId"] = json!(sid);
                    }
                    let _ = write.send(Message::Text(evt.to_string().into())).await;

                    // Network.loadingFinished for the document (real Chromium order:
                    // requestWillBeSent → responseReceived → loadingFinished). Carries
                    // `encodedDataLength` — the on-wire response byte count the shim
                    // records into LoomNetworkEvent.response_bytes (no getResponseBody
                    // round-trip). Fixed 1234 so the captured size is deterministic.
                    let mut doc_finished = json!({
                        "method": "Network.loadingFinished",
                        "params": {
                            "requestId": "fake-req-1",
                            "timestamp": 1.05,
                            "encodedDataLength": 1234,
                        },
                    });
                    if let Some(sid) = &session_id {
                        doc_finished["sessionId"] = json!(sid);
                    }
                    let _ = write
                        .send(Message::Text(doc_finished.to_string().into()))
                        .await;

                    // A known xhr to `/api/thing` — exercises the full-capture
                    // network-entries path (NON-Document, with method+status+
                    // resource_type) that the studio's route footprints need.
                    // Dropped by the Document-only `network_events` path, so it
                    // appears ONLY in `network_entries`.
                    let api_url = format!("{}/api/thing", nav_url.trim_end_matches('/'));
                    let mut xhr_req = json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "fake-xhr-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.1,
                            "wallTime": 1_700_000_001.0,
                            "type": "XHR",
                            "request": { "url": api_url, "method": "GET" },
                        },
                    });
                    if let Some(sid) = &session_id {
                        xhr_req["sessionId"] = json!(sid);
                    }
                    let _ = write.send(Message::Text(xhr_req.to_string().into())).await;

                    let mut xhr_resp = json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "fake-xhr-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.2,
                            "type": "XHR",
                            "response": {
                                "url": api_url,
                                "status": 200,
                                "statusText": "OK",
                                "mimeType": "application/json",
                            },
                        },
                    });
                    if let Some(sid) = &session_id {
                        xhr_resp["sessionId"] = json!(sid);
                    }
                    let _ = write.send(Message::Text(xhr_resp.to_string().into())).await;
                }
                FakeUrlPattern::Error(code) => {
                    let mut evt = json!({
                        "method": "Network.loadingFailed",
                        "params": {
                            "requestId": "fake-req-1",
                            "timestamp": 1.0,
                            "type": "Document",
                            "errorText": code,
                            "canceled": false,
                        },
                    });
                    if let Some(sid) = &session_id {
                        evt["sessionId"] = json!(sid);
                    }
                    let _ = write.send(Message::Text(evt.to_string().into())).await;
                }
                FakeUrlPattern::PageWithTracker => {
                    // Emit two `Fetch.requestPaused`
                    // events synthetically, mirroring chromium's CDP
                    // wire shape. ONLY when the host has issued
                    // `Fetch.enable` (real Chromium gates emission the
                    // same way; the `blocklist_enabled = false` path
                    // never sends `Fetch.enable`, so no Fetch events).
                    // The first carries the operator's primary URL with
                    // `resourceType=Document` → interceptor's frameId-
                    // based skip-gate lets it through. The second is a
                    // sub-resource on a blocklisted host
                    // (`*.google-analytics.com`) → interceptor must
                    // answer `Fetch.failRequest{ errorReason:
                    // "BlockedByClient"}` and record one BlockedEvent.
                    if fetch_enabled {
                        let mut doc_evt = json!({
                            "method": "Fetch.requestPaused",
                            "params": {
                                "requestId": "fake-fetch-doc-1",
                                "request": { "url": &nav_url, "method": "GET" },
                                "frameId": "fake-frame-1",
                                "resourceType": "Document"
                            }
                        });
                        if let Some(sid) = &session_id {
                            doc_evt["sessionId"] = json!(sid);
                        }
                        let _ = write.send(Message::Text(doc_evt.to_string().into())).await;

                        let ga_url = "https://www.google-analytics.com/analytics.js";
                        let mut ga_evt = json!({
                            "method": "Fetch.requestPaused",
                            "params": {
                                "requestId": "fake-fetch-ga-1",
                                "request": { "url": ga_url, "method": "GET" },
                                "frameId": "fake-frame-1",
                                "resourceType": "Script"
                            }
                        });
                        if let Some(sid) = &session_id {
                            ga_evt["sessionId"] = json!(sid);
                        }
                        let _ = write.send(Message::Text(ga_evt.to_string().into())).await;
                    }

                    // The document's Network.responseReceived feeds the
                    // action_executor's status_code derivation (mirrors
                    // the Status branch behavior). With the Fetch gate
                    // active the document is PAUSED: hold the event (and
                    // the load event, stashed below) until the client
                    // answers the pause — real Chromium does not let the
                    // navigation proceed past an unanswered Document
                    // pause. Without the gate, emit immediately.
                    let mut resp_evt = json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "fake-req-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.0,
                            "type": "Document",
                            "response": {
                                "url": &nav_url,
                                "status": 200,
                                "statusText": "OK",
                                "mimeType": "text/html",
                            },
                        },
                    });
                    if let Some(sid) = &session_id {
                        resp_evt["sessionId"] = json!(sid);
                    }
                    if fetch_enabled {
                        paused_doc = Some(PausedDoc {
                            request_id: "fake-fetch-doc-1".to_string(),
                            held_events: vec![resp_evt.to_string()],
                            load_event: None,
                        });
                    } else {
                        let _ = write.send(Message::Text(resp_evt.to_string().into())).await;
                    }
                }
                FakeUrlPattern::PageWithIframe404 => {
                    // Main document loads fine (200) under the navigation's
                    // frameId/loaderId (matching the canned Page.navigate
                    // response), while an embedded iframe's document 404s
                    // under its OWN frameId/loaderId. Real-Chromium shape:
                    // both are type=Document Network events on one target.
                    let mut main_req = json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "fake-req-1",
                            "frameId": "fake-frame-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.0,
                            "wallTime": 1_700_000_000.0,
                            "type": "Document",
                            "request": { "url": nav_url, "method": "GET" },
                        },
                    });
                    if let Some(sid) = &session_id {
                        main_req["sessionId"] = json!(sid);
                    }
                    let _ = write.send(Message::Text(main_req.to_string().into())).await;

                    let mut main_resp = json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "fake-req-1",
                            "frameId": "fake-frame-1",
                            "loaderId": "fake-loader-1",
                            "timestamp": 1.0,
                            "type": "Document",
                            "response": {
                                "url": nav_url,
                                "status": 200,
                                "statusText": "OK",
                                "mimeType": "text/html",
                            },
                        },
                    });
                    if let Some(sid) = &session_id {
                        main_resp["sessionId"] = json!(sid);
                    }
                    let _ = write
                        .send(Message::Text(main_resp.to_string().into()))
                        .await;

                    let iframe_url = "http://fake.test/embedded-iframe-404";
                    let mut iframe_req = json!({
                        "method": "Network.requestWillBeSent",
                        "params": {
                            "requestId": "fake-req-iframe-1",
                            "frameId": "fake-frame-iframe-1",
                            "loaderId": "fake-loader-iframe-1",
                            "timestamp": 1.1,
                            "wallTime": 1_700_000_001.0,
                            "type": "Document",
                            "request": { "url": iframe_url, "method": "GET" },
                        },
                    });
                    if let Some(sid) = &session_id {
                        iframe_req["sessionId"] = json!(sid);
                    }
                    let _ = write
                        .send(Message::Text(iframe_req.to_string().into()))
                        .await;

                    let mut iframe_resp = json!({
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": "fake-req-iframe-1",
                            "frameId": "fake-frame-iframe-1",
                            "loaderId": "fake-loader-iframe-1",
                            "timestamp": 1.2,
                            "type": "Document",
                            "response": {
                                "url": iframe_url,
                                "status": 404,
                                "statusText": "Not Found",
                                "mimeType": "text/html",
                            },
                        },
                    });
                    if let Some(sid) = &session_id {
                        iframe_resp["sessionId"] = json!(sid);
                    }
                    let _ = write
                        .send(Message::Text(iframe_resp.to_string().into()))
                        .await;
                }
                // The `/slow/<MS>` delay was already applied before the
                // navigate response above; from here it behaves like a plain
                // navigate (no synthetic network event).
                FakeUrlPattern::Slow(_) | FakeUrlPattern::None => {}
            }

            // Emit Page.loadEventFired after Page.navigate so the daemon's
            // wait-for-load-event doesn't time out. Real Chromium emits
            // this event on the page session's sessionId, not the
            // browser one. While the document is paused at the Fetch gate
            // the event is HELD with the pause (load cannot fire before
            // the navigation is even allowed to proceed); while the
            // virtual-time clock pin is in effect it is DEFERRED until the
            // next budget arm instead (see `vt_clock_paused` — mirrors real
            // headless Chromium, which holds load completion while virtual
            // time is not advancing).
            let mut evt = json!({
                "method": "Page.loadEventFired",
                "params": { "timestamp": 1.0 }
            });
            if let Some(sid) = &session_id {
                evt["sessionId"] = json!(sid);
            }
            if let Some(pd) = paused_doc.as_mut() {
                pd.load_event = Some(evt.to_string());
            } else if vt_clock_paused {
                deferred_load_event = Some(evt.to_string());
            } else {
                let _ = write.send(Message::Text(evt.to_string().into())).await;
            }
        }
    }
}

/// A document request held at the Fetch gate (PageWithTracker with
/// `Fetch.enable` issued). Carries the would-be navigation events until
/// the client answers `Fetch.continueRequest` (flush) or
/// `Fetch.failRequest` (abort with `Network.loadingFailed`).
struct PausedDoc {
    /// The paused Fetch requestId the client must answer.
    request_id: String,
    /// Held event frames (the document's `Network.responseReceived`).
    held_events: Vec<String>,
    /// The held `Page.loadEventFired` frame, populated when the navigate
    /// block reaches its load-event emission.
    load_event: Option<String>,
}

/// URL-driven test scaffolding for navigate-error and blocklist behaviours.
/// Real Chromium would fetch the URL from the network; fake-chromium
/// pattern-matches the URL string and emits the corresponding CDP events
/// synthetically.
enum FakeUrlPattern {
    /// `http://fake.test/status/<N>` → emit Network.responseReceived with
    /// the parsed HTTP status.
    Status(u16),
    /// `http://fake.test/error/<CDP>` → emit Network.loadingFailed AND
    /// set errorText=<CDP> in the Page.navigate response.
    Error(String),
    /// `http://fake.test/page-with-tracker` → after
    /// Page.navigate, emit `Fetch.requestPaused` events for the
    /// document AND for a hard-coded analytics sub-resource
    /// (`https://www.google-analytics.com/analytics.js`). The document
    /// event has `resourceType="Document"` and is first per-frame so
    /// the interceptor's frameId-based skip-gate lets it through; the
    /// sub-resource event has `resourceType="Script"` and matches the
    /// default blocklist → must be answered with `Fetch.failRequest`.
    PageWithTracker,
    /// `http://fake.test/page-with-iframe-404` → emit the MAIN
    /// document's 200 responseReceived (frameId/loaderId matching the
    /// canned `Page.navigate` response: fake-frame-1/fake-loader-1)
    /// PLUS an IFRAME document's 404 responseReceived under a
    /// different frameId/loaderId. The navigate must SUCCEED with
    /// status_code=200 — the iframe 404 stays in `network_events` for
    /// observability only (main-document failure scoping).
    PageWithIframe404,
    /// `http://fake.test/slow/<MS>` → sleep <MS> milliseconds BEFORE sending
    /// the `Page.navigate` response, so a shim per-CDP-command navigate-budget
    /// timeout (`LOOM_SHIM_CDP_TIMEOUT_MS`) fires deterministically. The delay
    /// is on the navigate command's own response — the binding CDP roundtrip.
    Slow(u64),
    /// Anything else — emit no synthetic Network event (status_code
    /// will remain 0 from the shim's perspective, mirroring real
    /// Chromium with caching disabled).
    None,
}

/// Build a synthetic `Runtime.evaluate` response body from a test-only
/// expression sentinel. The fake-chromium does NOT execute JS — it
/// pattern-matches the expression string and constructs the same shape
/// real Chromium would return for the equivalent successful or thrown
/// evaluation.
///
/// Sentinels (all start with `__loom_test_`):
///   `__loom_test_int__`              → result.value = 2 (integer)
///   `__loom_test_str__`              → result.value = "hello"
///   `__loom_test_doc_title__`        → result.value = "My Page"
///   `__loom_test_null__`             → result.value = null
///   `__loom_test_undef__`            → result.type = "undefined", no value
///   `__loom_test_empty_str__`        → result.value = ""
///   `__loom_test_pi__`               → result.value = 3.141592653589793 (float)
///   `__loom_test_obj__`              → result.value = {"label":"Click here","count":42}
///   `__loom_test_throw__:<MSG>`      → exceptionDetails with description Error: <MSG>
///   `__loom_test_large__:<SIZE_KB>`  → result.value = "x" repeated SIZE_KB×1024 times
///   `__loom_test_emit_doc_event__:<STATUS>` → result.value = 1; ALSO emits a
///       Document requestWillBeSent + responseReceived(status=STATUS) BEFORE
///       the response (handled in `handle_connection` — models a click-
///       triggered link navigation between navigates; stale-event regression)
///
/// Anything else → empty `{}` (caller treats as a no-op evaluate).
/// voice-call-io task 07: answer the four nonce'd in-page audio API calls the
/// `AudioBridge` drives (`this.enqueue(` / `this.startCapture(` / `this.stopCapture(`
/// / `this.drain(`), using env-scripted synthetic data. The fake stays DUMB
/// (Architecture A17): echo replays the injected base64 verbatim, tone emits a canned
/// sine, no-gum rejects. This is NOT a general `Runtime.callFunctionOn` interpreter —
/// any other function declaration returns `None`, so non-audio callFunctionOn (e.g.
/// frame-locator resolution) falls through to the unchanged `canned_response`.
///
/// `echo_b64` is the per-connection echo buffer (set on enqueue, taken on the first
/// drain, then `None` → later drains return an empty `more:false` payload).
/// The objectId `build_fake_evaluate_response` hands back for `window.__loom_<nonce>`.
const FAKE_AUDIO_API_OBJECT_ID: &str = "fake-audio-api-1";

fn audio_call_function_on(params: &Value, echo_b64: &mut Option<String>) -> Option<Value> {
    // Scope strictly to calls ON the resolved audio API object — a non-audio
    // `callFunctionOn` (e.g. frame-locator resolution) targets a different objectId
    // and must fall through to `canned_response` unchanged, even if its function text
    // coincidentally contains one of the audio tokens below.
    if params.get("objectId").and_then(|v| v.as_str()) != Some(FAKE_AUDIO_API_OBJECT_ID) {
        return None;
    }
    let decl = params
        .get("functionDeclaration")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let no_gum = env_flag("LOOM_FAKE_CHROMIUM_AUDIO_NO_GUM");
    let echo = env_flag("LOOM_FAKE_CHROMIUM_AUDIO_ECHO");
    let tone = std::env::var("LOOM_FAKE_CHROMIUM_AUDIO_TONE").ok();

    if decl.contains("this.enqueue(") {
        if no_gum {
            // AC10: the page never called getUserMedia → enqueue rejects. The shim's
            // `map_enqueue_exception` maps a description containing `no_microphone_request`
            // to the typed kind.
            return Some(json!({
                "exceptionDetails": {
                    "exception": { "description": "Error: no_microphone_request" }
                }
            }));
        }
        if echo {
            // Stash the injected base64 (arguments[0].value) to replay out of drain.
            *echo_b64 = params
                .pointer("/arguments/0/value")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        // enqueue resolves with the clip duration in seconds; the value is
        // observational only (the shim tolerates its absence) → 0.0.
        return Some(json!({ "result": { "type": "number", "value": 0.0 } }));
    }

    if decl.contains("this.startCapture(") || decl.contains("this.stopCapture(") {
        return Some(json!({ "result": { "value": { "ok": true } } }));
    }

    if decl.contains("this.drain(") {
        // Build the drained payload the shim's `parse_drain` expects. samples_b64 is
        // base64 of little-endian f32 native PCM.
        let (samples_b64, sample_rate, tapped) = if echo {
            match echo_b64.take() {
                Some(b64) => (b64, 16_000u32, 1u32), // 16 kHz → shim resample is passthrough (D1)
                None => (String::new(), 0, 0),       // already drained → empty
            }
        } else if let Some(spec) = tone {
            // `freq:ms` → a canned sine at the native 48 kHz AudioContext rate (D2),
            // so the shim runs a REAL 48k→16k resample end to end.
            match parse_tone_spec(&spec) {
                Some((freq, ms)) => (sine_f32_le_b64(freq, ms, 48_000), 48_000, 1),
                None => (String::new(), 0, 0),
            }
        } else {
            (String::new(), 0, 0)
        };
        return Some(json!({
            "result": { "value": {
                "samples_b64": samples_b64,
                "sample_rate": sample_rate,
                "dropped_frames": 0,
                "tapped_tracks": tapped,
                "injected_leaked": false,
                "buffer_cap_hit": false,
                "more": false,
            }}
        }));
    }

    None
}

/// Env boolean: set and equal to `1`/`true` (case-insensitive).
fn env_flag(key: &str) -> bool {
    matches!(std::env::var(key), Ok(v) if v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Parse a `LOOM_FAKE_CHROMIUM_AUDIO_TONE=<freq>:<ms>` spec into `(freq_hz, ms)`.
fn parse_tone_spec(spec: &str) -> Option<(f64, u64)> {
    let (f, ms) = spec.split_once(':')?;
    Some((f.trim().parse().ok()?, ms.trim().parse().ok()?))
}

/// `rate*ms/1000` samples of `0.5*sin(2π·freq·t)`, little-endian f32, STANDARD base64.
/// Bounded by design (callers use ≤ 100 ms), so a single drain stays well under the
/// shim's per-call byte guard.
fn sine_f32_le_b64(freq: f64, ms: u64, rate: u32) -> String {
    use base64::Engine as _;
    // Bound the allocation so a fat-fingered `AUDIO_TONE=440:99999999` can't OOM/stall
    // CI: saturating math (no wrap) + a 10 s ceiling (well past any real test tone).
    let n = (u64::from(rate).saturating_mul(ms) / 1000).min(u64::from(rate) * 10) as usize;
    let mut bytes = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let s = (0.5 * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32;
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

fn build_fake_evaluate_response(expression: &str) -> Value {
    // settle-capture: the ReadinessMonitor settle probe (identified by its
    // unique `__loomSettleMut` global) is intercepted in `handle_connection`
    // BEFORE this function is reached, because its per-tick response is driven
    // by mutable per-connection state (the script index) + the optional
    // `LOOM_FAKE_CHROMIUM_SCRIPT`. See `SettleScript::probe_response`.

    // Throw sentinel:  __loom_test_throw__:<message>
    if let Some(msg) = expression.strip_prefix("__loom_test_throw__:") {
        return json!({
            "result": {
                "type": "object",
                "subtype": "error",
                "className": "Error",
            },
            "exceptionDetails": {
                "exceptionId": 1,
                "text": "Uncaught",
                "lineNumber": 0,
                "columnNumber": 6,
                "scriptId": "1",
                "exception": {
                    "type": "object",
                    "subtype": "error",
                    "className": "Error",
                    "description": format!("Error: {msg}"),
                },
            },
        });
    }

    // Large sentinel:  __loom_test_large__:<size_kb>
    if let Some(rest) = expression.strip_prefix("__loom_test_large__:") {
        let kb: usize = rest.parse().unwrap_or(80);
        // Build a string of EXACT length `kb * 1024 - 2` so the
        // canonical-JSON encoding (which adds two surrounding quotes)
        // is exactly `kb * 1024` bytes. This lets boundary tests target
        // 65_535 / 65_536 / 65_537 byte canonical-JSON outputs precisely.
        let target_chars = kb.saturating_mul(1024).saturating_sub(2);
        let big = "x".repeat(target_chars);
        return json!({
            "result": {
                "type": "string",
                "value": big,
            },
        });
    }

    // Stale-event injection sentinel:  __loom_test_emit_doc_event__:<status>
    // The Document-event emission happens in `handle_connection` (it needs
    // the websocket writer); here we just return a successful scalar so the
    // evaluate round-trip completes cleanly.
    if expression.starts_with("__loom_test_emit_doc_event__:") {
        return json!({
            "result": { "type": "number", "value": 1 },
        });
    }

    // Bytes-target sentinel:  __loom_test_size__:<bytes>
    // Produces canonical-JSON of exactly <bytes> length (string value
    // wrapped in two quote chars, so the inner string is bytes-2 chars).
    if let Some(rest) = expression.strip_prefix("__loom_test_size__:") {
        let bytes: usize = rest.parse().unwrap_or(65_536);
        let target_chars = bytes.saturating_sub(2);
        let big = "x".repeat(target_chars);
        return json!({
            "result": {
                "type": "string",
                "value": big,
            },
        });
    }

    // voice-call-io task 07: the AudioBridge resolves the per-session nonce'd
    // in-page API object via `Runtime.evaluate window.__loom_<nonce>` (returnByValue
    // false) before every enqueue/startCapture/drain callFunctionOn. Return a stable
    // objectId so the resolve succeeds for any `--audio` session. The api object is
    // "present" regardless of AUDIO_NO_GUM — the missing-mic case (AC10) rejects at
    // `enqueue`, not at resolve.
    if expression.starts_with("window.__loom_") {
        return json!({
            "result": { "type": "object", "objectId": FAKE_AUDIO_API_OBJECT_ID },
        });
    }

    match expression {
        "__loom_test_int__" => json!({
            "result": { "type": "number", "value": 2 },
        }),
        "__loom_test_str__" => json!({
            "result": { "type": "string", "value": "hello" },
        }),
        "__loom_test_doc_title__" => json!({
            "result": { "type": "string", "value": "My Page" },
        }),
        "__loom_test_null__" => json!({
            "result": { "type": "object", "subtype": "null", "value": Value::Null },
        }),
        "__loom_test_undef__" => json!({
            // CDP returns no `value` field for undefined.
            "result": { "type": "undefined" },
        }),
        "__loom_test_empty_str__" => json!({
            "result": { "type": "string", "value": "" },
        }),
        "__loom_test_pi__" => {
            // Real Chromium returns floats verbatim in the value field.
            // Use std::f64::consts::PI to satisfy clippy::approx_constant.
            json!({
                "result": { "type": "number", "value": std::f64::consts::PI },
            })
        }
        "__loom_test_obj__" => json!({
            "result": {
                "type": "object",
                "value": { "label": "Click here", "count": 42 },
            },
        }),
        _ => json!({}),
    }
}

fn parse_fake_url_pattern(url: &str) -> FakeUrlPattern {
    if let Some(rest) = url
        .strip_prefix("http://fake.test/status/")
        .or_else(|| url.strip_prefix("https://fake.test/status/"))
    {
        let n: &str = rest.split('?').next().unwrap_or("");
        if let Ok(status) = n.parse::<u16>() {
            return FakeUrlPattern::Status(status);
        }
    }
    if let Some(rest) = url
        .strip_prefix("http://fake.test/error/")
        .or_else(|| url.strip_prefix("https://fake.test/error/"))
    {
        let code = rest.split('?').next().unwrap_or("");
        if !code.is_empty() {
            return FakeUrlPattern::Error(code.to_string());
        }
    }
    if let Some(rest) = url
        .strip_prefix("http://fake.test/slow/")
        .or_else(|| url.strip_prefix("https://fake.test/slow/"))
    {
        let n: &str = rest.split('?').next().unwrap_or("");
        if let Ok(ms) = n.parse::<u64>() {
            return FakeUrlPattern::Slow(ms);
        }
    }
    if url == "http://fake.test/page-with-tracker" || url == "https://fake.test/page-with-tracker" {
        return FakeUrlPattern::PageWithTracker;
    }
    // BLOCKLISTED-host variant: same tracker page served from a host that
    // matches the default blocklist (`*.google-analytics.com`). The
    // document's own Fetch.requestPaused URL then matches the blocklist,
    // exercising the interceptor's main-frame skip-gate — the documented
    // 'operator's primary URL is never gated' invariant — on EVERY
    // navigate of a session, not just the first.
    if url == "https://www.google-analytics.com/page-with-tracker" {
        return FakeUrlPattern::PageWithTracker;
    }
    if url == "http://fake.test/page-with-iframe-404"
        || url == "https://fake.test/page-with-iframe-404"
    {
        return FakeUrlPattern::PageWithIframe404;
    }
    FakeUrlPattern::None
}

/// Local copy of the browser-scope classifier so fake-chromium doesn't
/// need to depend on loom-shims' library code (it's a separate bin
/// target). Stays in sync with `is_browser_scope_method` in
/// `loom-shims/src/cdp_connection/interfaces.rs`.
fn is_browser_scope_method_local(method: &str) -> bool {
    matches!(
        method.split('.').next().unwrap_or(""),
        "Browser" | "Target" | "Tracing" | "Storage" | "Schema" | "SystemInfo" | "Memory"
    )
}

/// Canned CDP-method responses. Best-effort — unknown methods get an
/// empty `{}` result so the daemon doesn't error out on routine calls.
///
/// `params` is consumed for `DOM.querySelector` (extract `selector`) and
/// `DOM.getBoxModel` (extract `nodeId`); for other methods it is ignored.
/// A per-process, per-call ephemeral frame id — stands in for the random
/// per-navigation `frameId` real Chromium embeds in `DOM.getDocument`. Distinct
/// across independent runs so a content-stable `dom_snapshot_hash` can only hold
/// if the shim strips it.
fn ephemeral_frame_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "fake-frame-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn canned_response(method: &str, params: &Value) -> Value {
    match method {
        "Page.navigate" => json!({
            "frameId": "fake-frame-1",
            "loaderId": "fake-loader-1"
        }),
        "DOM.getDocument" => {
            // Honor `pierce`: real Chromium inlines shadow-DOM + iframe
            // contentDocument subtrees only when pierce:true. Each inlined document
            // carries its OWN ephemeral frameId. Node ids are STABLE synthetic
            // values; only the frameIds vary per call, so two captures of the same
            // tree normalize (frameId stripped recursively) to identical bytes —
            // which the pierced-path determinism e2e asserts.
            //
            // This fixture validates the normalization plumbing ONLY. It does NOT
            // reproduce browser-enforced same-origin / CORS isolation that real
            // Chromium applies to pierced subtrees.
            let pierce = params
                .get("pierce")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            if pierce {
                json!({
                    "root": {
                        "nodeId": 1,
                        "backendNodeId": 1,
                        "nodeName": "#document",
                        "nodeType": 9,
                        "childNodeCount": 2,
                        "frameId": ephemeral_frame_id(),
                        "children": [
                            // Shadow host — its shadowRoot subtree is inlined under pierce.
                            {
                                "nodeId": 2,
                                "backendNodeId": 2,
                                "nodeName": "DIV",
                                "nodeType": 1,
                                "shadowRoots": [
                                    {
                                        "nodeId": 3,
                                        "backendNodeId": 3,
                                        "nodeName": "#document-fragment",
                                        "nodeType": 11,
                                        "children": [
                                            {
                                                "nodeId": 4,
                                                "backendNodeId": 4,
                                                "nodeName": "SPAN",
                                                "nodeType": 1,
                                                "children": []
                                            }
                                        ]
                                    }
                                ],
                                "children": []
                            },
                            // Iframe — its contentDocument is inlined under pierce,
                            // each level carrying its own ephemeral frameId.
                            {
                                "nodeId": 5,
                                "backendNodeId": 5,
                                "nodeName": "IFRAME",
                                "nodeType": 1,
                                "frameId": ephemeral_frame_id(),
                                "contentDocument": {
                                    "nodeId": 6,
                                    "backendNodeId": 6,
                                    "nodeName": "#document",
                                    "nodeType": 9,
                                    "frameId": ephemeral_frame_id(),
                                    "children": [
                                        {
                                            "nodeId": 7,
                                            "backendNodeId": 7,
                                            "nodeName": "BODY",
                                            "nodeType": 1,
                                            "children": []
                                        }
                                    ]
                                }
                            }
                        ]
                    }
                })
            } else {
                json!({
                    "root": {
                        "nodeId": 1,
                        "backendNodeId": 1,
                        "nodeName": "#document",
                        "nodeType": 9,
                        "childNodeCount": 0,
                        // Ephemeral per-navigation frame id, mirroring real Chromium.
                        // Varies per call + per process so the determinism e2e proves
                        // `dom_snapshot_hash` normalization STRIPS it: two independent
                        // same-seed runs hash identically ONLY because the shim removes
                        // this id (see loom_shared::dom_normalize).
                        "frameId": ephemeral_frame_id(),
                        "children": []
                    }
                })
            }
        }
        "DOM.querySelector" => {
            let sel = params
                .get("selector")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let node_id = dom_fixture().ids_by_selector.get(sel).copied().unwrap_or(0);
            json!({ "nodeId": node_id })
        }
        "DOM.scrollIntoViewIfNeeded" => json!({}),
        // web.type fill prepare step (fixture `inputs`): resolveNode → a
        // `fake-node:<nodeId>` handle; callFunctionOn on it → the scripted verdict.
        "DOM.resolveNode" => {
            let node_id = params.get("nodeId").and_then(|n| n.as_u64()).unwrap_or(0);
            match dom_fixture().selectors_by_id.get(&node_id) {
                None => json!({
                    "__cdp_error__": { "code": -32000, "message": "No node with given id found" }
                }),
                Some(sel) => match dom_fixture().inputs.get(sel).map(String::as_str) {
                    Some("resolve_error") => json!({
                        "__cdp_error__": { "code": -32000, "message": "Node is not resolvable" }
                    }),
                    Some("no_object") => json!({ "object": { "type": "undefined" } }),
                    _ => json!({
                        "object": {
                            "type": "object",
                            "subtype": "node",
                            "objectId": format!("fake-node:{node_id}"),
                        }
                    }),
                },
            }
        }
        "Runtime.callFunctionOn" => {
            let node_id = params
                .get("objectId")
                .and_then(|o| o.as_str())
                .and_then(|o| o.strip_prefix("fake-node:"))
                .and_then(|n| n.parse::<u64>().ok());
            let verdict = node_id
                .and_then(|n| dom_fixture().selectors_by_id.get(&n))
                .and_then(|sel| dom_fixture().inputs.get(sel))
                .map(String::as_str)
                .unwrap_or("insert");
            let by_value = |value: Value| json!({ "result": { "type": "object", "value": value } });
            match verdict {
                "throw" => json!({
                    "result": { "type": "object", "subtype": "error" },
                    "exceptionDetails": {
                        "text": "Uncaught",
                        "exception": { "description": "Error: fake page exception" }
                    }
                }),
                "garbage" => by_value(json!({ "v": "sideways" })),
                other => match other.strip_prefix("malformed:") {
                    Some(input_type) => by_value(json!({ "v": "malformed", "type": input_type })),
                    None => by_value(json!({ "v": other })),
                },
            }
        }
        "Runtime.releaseObjectGroup" => {
            if dom_fixture().release_error {
                json!({ "__cdp_error__": { "code": -32000, "message": "fake release failure" } })
            } else {
                json!({})
            }
        }
        // cdp-trusted-input: focus + real CDP input dispatch. The fake doesn't
        // model keyboard/mouse state — it just acks (empty success), which is
        // enough to assert the host issues the right CDP envelopes and records
        // the trusted-input receipt.
        "DOM.focus" => json!({}),
        "Input.dispatchKeyEvent" => json!({}),
        "Input.insertText" => json!({}),
        "Input.dispatchMouseEvent" => json!({}),
        "DOM.getBoxModel" => {
            let nid = params.get("nodeId").and_then(|n| n.as_u64()).unwrap_or(0);
            let fixture = dom_fixture();
            match fixture
                .selectors_by_id
                .get(&nid)
                .and_then(|sel| fixture.boxes.get(sel))
            {
                Some(b) => {
                    let [x1, y1, x2, y2] = *b;
                    if (x2 - x1) <= 0.0 || (y2 - y1) <= 0.0 {
                        // Real Chromium returns -32000 for zero-area /
                        // hidden / detached elements. The hit_test helper
                        // maps any DOM.getBoxModel error to
                        // ShimFailureKind::HitTestFailed.
                        return json!({
                            "__cdp_error__": {
                                "code": -32000,
                                "message": "Could not compute box model.",
                            }
                        });
                    }
                    let w = (x2 - x1) as u64;
                    let h = (y2 - y1) as u64;
                    json!({
                        "model": {
                            "content": [x1, y1, x2, y1, x2, y2, x1, y2],
                            "padding": [x1, y1, x2, y1, x2, y2, x1, y2],
                            "border":  [x1, y1, x2, y1, x2, y2, x1, y2],
                            "margin":  [x1, y1, x2, y1, x2, y2, x1, y2],
                            "width":  w,
                            "height": h,
                        }
                    })
                }
                None => json!({
                    "__cdp_error__": {
                        "code": -32000,
                        "message": "Could not find node with given id",
                    }
                }),
            }
        }
        "Page.getLayoutMetrics" => {
            let [w, h] = dom_fixture().viewport;
            json!({
                "layoutViewport":   { "pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": h },
                "visualViewport":   { "offsetX": 0.0, "offsetY": 0.0, "pageX": 0.0, "pageY": 0.0, "clientWidth": w, "clientHeight": h, "scale": 1.0, "zoom": 1.0 },
                "cssLayoutViewport":{ "pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": h },
                "contentSize":      { "x": 0.0, "y": 0.0, "width": w, "height": h },
                "cssContentSize":   { "x": 0.0, "y": 0.0, "width": w, "height": h },
            })
        }
        "Page.captureScreenshot" => json!({
            // 1×1 transparent PNG, base64-encoded.
            "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII="
        }),
        "Page.addScriptToEvaluateOnNewDocument" => json!({
            "identifier": "1"
        }),
        "Target.createTarget" => json!({
            "targetId": "fake-target-1"
        }),
        "Target.attachToTarget" => json!({
            // Stable id so integration tests can assert echo-back.
            "sessionId": "fake-session-1"
        }),
        "Page.enable" => json!({}),
        "Network.enable" => json!({}),
        "DOM.enable" => json!({}),
        "Runtime.enable" => json!({}),
        // Fetch domain methods used by
        // the network_interceptor's blocklist gate. All return empty
        // success per CDP convention.
        "Fetch.enable" => json!({}),
        "Fetch.continueRequest" => json!({}),
        "Fetch.failRequest" => json!({}),
        "Target.getTargets" => json!({
            "targetInfos": []
        }),
        // Page.enable, Network.enable, DOM.enable, etc.
        _ => json!({}),
    }
}

/// settle-capture: deterministic per-tick script for the readiness probe,
/// loaded once from `LOOM_FAKE_CHROMIUM_SCRIPT`. Absent env → a single
/// "immediately settled" entry, reproducing the legacy hard-coded probe
/// response (`[true,"https://fake.test/",0]`) so every non-settle test is
/// unaffected.
#[derive(Debug, Clone)]
struct SettleScript {
    /// Per-tick probe responses `(ready_complete, href, dom_mutations)`.
    /// Never empty. The last entry repeats once the cursor runs off the end.
    probe: Vec<(bool, String, u32)>,
    /// Number of never-finishing in-flight requests to pin (the never-settles
    /// network shape). Zero for normal pages.
    perpetual_inflight: usize,
    /// Probe-tick indices at which the loaded PAGE begins a fresh top-level
    /// navigation it initiated itself (window.location / <meta refresh> /
    /// form-POST). Models the client-side-redirect bug: when the cursor
    /// reaches one of these ticks, the fake queues a NEW `Page.loadEventFired`
    /// gated on the NEXT virtual-time budget arm (real headless Chromium holds
    /// the new document's load while the clock is paused) and pins the probe to
    /// `readyState:"loading"` until the executor re-arms the budget. An
    /// executor that never re-attaches (the bug) stays wedged on the blank
    /// in-flight document exactly like it does against real Chromium; one that
    /// re-arms + re-settles reaches the final page. The href reported while
    /// loading is taken from `probe[idx]`.
    renavigate_at: Vec<usize>,
    /// Probe-tick indices at which the click/nav triggers a CROSS-ORIGIN
    /// top-level navigation whose renderer PROCESS SWAPS. Unlike
    /// `renavigate_at` (a same-process client redirect the stale CDP session
    /// keeps observing), a cross-process swap means the virtual-time budget
    /// armed on the OLD session never produces `virtualTimeBudgetExpired` and
    /// the deferred `Page.loadEventFired` never flushes on the stale
    /// connection — both events land on a new renderer the shim is not
    /// watching. Modeled by entering the `awaiting_rearm`/"loading" gate like
    /// `renavigate_at`, but then SUPPRESSING the load-flush + budget-expiry on
    /// the subsequent re-arm (never-recovering: the stale session stays blind).
    /// This is the adversarial case that reproduces the multi-phase dead-wait;
    /// a correct executor still returns a bounded, typed `timeout` inside its
    /// single shared deadline.
    cross_origin_swap_at: Vec<usize>,
    /// When true, the fake WITHHOLDS the CDP response for the trusted-click
    /// commit event (`Input.dispatchMouseEvent` with `type:"mousePressed"`) —
    /// modelling a cross-origin renderer PROCESS SWAP that tears the stale CDP
    /// session down before the mouse-dispatch ack is written, so the ack never
    /// reaches the host. Reproduces the click-DISPATCH dead-wait (distinct from
    /// `cross_origin_swap_at`, which only gates the later settle probe). A
    /// correct host bounds the dispatch by its budget and returns a typed
    /// "dispatched" outcome (the event WAS sent) instead of dead-waiting the
    /// full recv timeout. `mouseMoved`/`mouseReleased` are still acked so only
    /// the commit event's ack is lost.
    swallow_dispatch_ack: bool,
    /// When true, the fake WITHHOLDS the CDP response for EVERY trusted-click
    /// mouse frame from `mouseMoved` onward (`mouseMoved` + `mousePressed` +
    /// `mouseReleased`) — modelling a cross-origin renderer PROCESS SWAP whose
    /// window opens at/before the move, so the stale CDP session goes blind before
    /// even the PRE-COMMIT (`mouseMoved`, index 0) ack is written. This exercises
    /// the surviving `input dispatch ack timeout before commit` arm that
    /// `swallow_dispatch_ack` (commit-frame only) cannot reach: a correct host must
    /// keep dispatching the committing frames and report a typed "dispatched"
    /// outcome, never a transport `shim_timeout`.
    swallow_dispatch_ack_from_move: bool,
    /// When true, withhold ONLY the PRE-COMMIT (`mouseMoved`) ack; the committing
    /// frames still ack. Models a transient move-ack drop on a LIVE session — the
    /// click still commits, so a correct host returns `Ok` (a dropped move ack
    /// alone must not degrade a fully-committed click to `DispatchedAckPending`).
    swallow_dispatch_ack_move_only: bool,
    /// When true, the committing frame (`Input.dispatchMouseEvent
    /// type:"mousePressed"`) responds with a CDP application ERROR envelope rather
    /// than an ack — a GENUINE dispatch failure (renderer rejects the input),
    /// distinct from a swap ack loss. A correct host surfaces it as a hard failure
    /// (`Some(Err)` arm of `dispatch_input_events`), never a performed click.
    dispatch_cdp_error: bool,
    /// Delay (ms) before acking the trusted-click commit event
    /// (`Input.dispatchMouseEvent type:"mousePressed"`), modelling real Chrome's
    /// browser-process ack round-trip (tens of ms) rather than the fake's usual
    /// in-process instant ack. `0` = instant. Lets a test exercise a
    /// delayed-but-ARRIVING ack: it lands within the dispatch budget → `Ok`, NOT
    /// `DispatchedAckPending` — the regression guard for a dispatch budget
    /// collapsed below real ack latency.
    dispatch_ack_delay_ms: u64,
}

impl SettleScript {
    /// Build the `Runtime.evaluate` response for the settle probe at tick
    /// `idx`. `result.value` is the JSON string `[ready, "href", mutations]`
    /// the host's `parse_probe` expects.
    fn probe_response(&self, idx: usize) -> Value {
        let (ready, href, muts) = self
            .probe
            .get(idx)
            .or_else(|| self.probe.last())
            .cloned()
            .unwrap_or_else(|| (true, "https://fake.test/".to_string(), 0));
        let encoded = json!([ready, href, muts]).to_string();
        json!({ "result": { "type": "string", "value": encoded } })
    }
}

fn default_settle_script() -> SettleScript {
    SettleScript {
        probe: vec![(true, "https://fake.test/".to_string(), 0)],
        perpetual_inflight: 0,
        renavigate_at: Vec::new(),
        cross_origin_swap_at: Vec::new(),
        swallow_dispatch_ack: false,
        swallow_dispatch_ack_from_move: false,
        swallow_dispatch_ack_move_only: false,
        dispatch_cdp_error: false,
        dispatch_ack_delay_ms: 0,
    }
}

static SETTLE_SCRIPT: OnceLock<SettleScript> = OnceLock::new();

fn settle_script() -> &'static SettleScript {
    SETTLE_SCRIPT.get_or_init(load_settle_script)
}

fn load_settle_script() -> SettleScript {
    let default = default_settle_script();
    let path = match std::env::var("LOOM_FAKE_CHROMIUM_SCRIPT") {
        Ok(p) => p,
        Err(_) => return default,
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fake-chromium: cannot read settle script {path}: {e}");
            return default;
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fake-chromium: malformed settle script JSON: {e}");
            return default;
        }
    };
    let probe: Vec<(bool, String, u32)> = v
        .get("settle_probe")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let e = e.as_array()?;
                    let ready = e.first()?.as_bool()?;
                    let href = e.get(1)?.as_str()?.to_string();
                    let muts = e.get(2)?.as_u64()? as u32;
                    Some((ready, href, muts))
                })
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or(default.probe);
    let perpetual_inflight = v
        .get("perpetual_inflight")
        .and_then(|n| n.as_u64())
        .unwrap_or(0) as usize;
    let renavigate_at: Vec<usize> = v
        .get("renavigate_at")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_u64().map(|n| n as usize))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cross_origin_swap_at: Vec<usize> = v
        .get("cross_origin_swap_at")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_u64().map(|n| n as usize))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let swallow_dispatch_ack = v
        .get("swallow_dispatch_ack")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let swallow_dispatch_ack_from_move = v
        .get("swallow_dispatch_ack_from_move")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let swallow_dispatch_ack_move_only = v
        .get("swallow_dispatch_ack_move_only")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let dispatch_cdp_error = v
        .get("dispatch_cdp_error")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let dispatch_ack_delay_ms = v
        .get("dispatch_ack_delay_ms")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    SettleScript {
        probe,
        perpetual_inflight,
        renavigate_at,
        cross_origin_swap_at,
        swallow_dispatch_ack,
        swallow_dispatch_ack_from_move,
        swallow_dispatch_ack_move_only,
        dispatch_cdp_error,
        dispatch_ack_delay_ms,
    }
}

/// True when the fixture scripts this fill prepare message's ack to be lost:
/// `DOM.resolveNode` for a node whose verdict is `swallow_resolve`, or
/// `Runtime.callFunctionOn` on the `fake-node:` handle of a `swallow_call` node.
fn fill_prepare_ack_swallowed(method: &str, params: &Value) -> bool {
    let (node_id, wanted) = match method {
        "DOM.resolveNode" => (
            params.get("nodeId").and_then(|n| n.as_u64()),
            "swallow_resolve",
        ),
        "Runtime.callFunctionOn" => (
            params
                .get("objectId")
                .and_then(|o| o.as_str())
                .and_then(|o| o.strip_prefix("fake-node:"))
                .and_then(|n| n.parse::<u64>().ok()),
            "swallow_call",
        ),
        _ => return false,
    };
    node_id
        .and_then(|n| dom_fixture().selectors_by_id.get(&n))
        .and_then(|sel| dom_fixture().inputs.get(sel))
        .is_some_and(|verdict| verdict == wanted)
}

/// Tiny DOM model used by the hit-test integration tests. Read once from
/// `LOOM_FAKE_CHROMIUM_FIXTURE` (a path to a JSON file). Empty when
/// unset.
#[derive(Debug, Clone, Default)]
struct DomFixture {
    /// Selector → bounding-box `[x1, y1, x2, y2]` in CSS pixels
    /// (top-left + bottom-right, axis-aligned).
    boxes: HashMap<String, [f64; 4]>,
    /// Selector → assigned synthetic `nodeId`.
    ids_by_selector: HashMap<String, u64>,
    /// Reverse: synthetic `nodeId` → selector.
    selectors_by_id: HashMap<u64, String>,
    /// Viewport `[width, height]` in CSS pixels. Defaults to `[1024, 768]`.
    viewport: [u64; 2],
    /// Selector → scripted `web.type` fill prepare verdict (see module docs).
    inputs: HashMap<String, String>,
    /// `Runtime.releaseObjectGroup` answers with a CDP error.
    release_error: bool,
}

static FIXTURE: OnceLock<DomFixture> = OnceLock::new();

fn dom_fixture() -> &'static DomFixture {
    FIXTURE.get_or_init(load_dom_fixture)
}

fn load_dom_fixture() -> DomFixture {
    let mut out = DomFixture {
        viewport: [1024, 768],
        ..Default::default()
    };
    let path = match std::env::var("LOOM_FAKE_CHROMIUM_FIXTURE") {
        Ok(p) => p,
        Err(_) => return out,
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fake-chromium: cannot read fixture {path}: {e}");
            return out;
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fake-chromium: malformed fixture JSON: {e}");
            return out;
        }
    };
    if let Some(vp) = v.get("viewport").and_then(|x| x.as_array()) {
        if vp.len() == 2 {
            if let (Some(w), Some(h)) = (vp[0].as_u64(), vp[1].as_u64()) {
                out.viewport = [w, h];
            }
        }
    }
    if let Some(inputs) = v.get("inputs").and_then(|i| i.as_object()) {
        for (sel, verdict) in inputs {
            if let Some(verdict) = verdict.as_str() {
                out.inputs.insert(sel.clone(), verdict.to_string());
            }
        }
    }
    out.release_error = v
        .get("release_error")
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    if let Some(boxes_obj) = v.get("boxes").and_then(|b| b.as_object()) {
        // Stable id assignment: deterministic ordering by selector string.
        let mut keys: Vec<&String> = boxes_obj.keys().collect();
        keys.sort();
        for (i, sel) in keys.iter().enumerate() {
            let arr = match boxes_obj.get(*sel).and_then(|x| x.as_array()) {
                Some(a) if a.len() == 4 => a,
                _ => continue,
            };
            let coords = match (
                arr[0].as_f64(),
                arr[1].as_f64(),
                arr[2].as_f64(),
                arr[3].as_f64(),
            ) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => [x1, y1, x2, y2],
                _ => continue,
            };
            // Synthetic ids start at 1000 to avoid colliding with the
            // root document nodeId (1).
            let node_id = 1000_u64 + (i as u64);
            out.boxes.insert((*sel).clone(), coords);
            out.ids_by_selector.insert((*sel).clone(), node_id);
            out.selectors_by_id.insert(node_id, (*sel).clone());
        }
    }
    out
}
