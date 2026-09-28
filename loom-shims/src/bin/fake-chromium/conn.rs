//! One CDP connection: the per-connection state real Chromium keeps, and the
//! request dispatcher that answers each message.

use futures::stream::SplitSink;
use futures::SinkExt;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::audio::audio_call_function_on;
use crate::canned::canned_response;
use crate::evaluate::build_fake_evaluate_response;
use crate::fetch_gate::PausedDoc;
use crate::settle_script::settle_script;
use crate::url_pattern::{parse_fake_url_pattern, FakeUrlPattern};

pub(crate) type WsWrite = SplitSink<WebSocketStream<TcpStream>, Message>;

/// The peer hung up: a frame could not be written, so the connection ends.
pub(crate) struct Closed;

/// Per-connection state. Each field mirrors a piece of state real Chromium
/// keeps per session, so a host regression diverges here exactly like it
/// would against the real browser.
pub(crate) struct Conn {
    pub(crate) write: WsWrite,
    /// When LOOM_FAKE_CHROMIUM_REQUIRE_SESSION_ID=1, validate
    /// that every page-scope CDP request carries a top-level `sessionId`
    /// field. Real Chromium rejects page-scope methods without sessionId
    /// with `{code:-32601, message:"'<method>' wasn't found"}`; this flag
    /// makes the integration test enforce the same constraint.
    pub(crate) require_session_id: bool,
    /// Real Chromium only delivers `Fetch.requestPaused` events AFTER the
    /// host issues `Fetch.enable`. Mirror that here so the synthetic
    /// PageWithTracker emission below doesn't fire when the host has
    /// disabled the blocklist gate (`blocklist_enabled = false` →
    /// `subscribe()` skipped → no `Fetch.enable`).
    pub(crate) fetch_enabled: bool,
    /// settle-capture: per-connection cursor into LOOM_FAKE_CHROMIUM_SCRIPT's
    /// `settle_probe` array. Reset on each Page.navigate so every navigation
    /// replays the script from the top; advanced once per settle probe.
    pub(crate) settle_idx: usize,
    /// Virtual-time fidelity: real headless Chromium DEFERS the load-completion
    /// tasks while virtual time is not advancing — after the inject-time
    /// `setVirtualTimePolicy {policy:"pause"}` pin, `Page.loadEventFired` is
    /// held until a budget-carrying setVirtualTimePolicy advances the clock
    /// (and the clock pauses again once each budget drains). Mirror that:
    /// once a pause-pin has been seen, every navigate's loadEventFired is
    /// deferred until the next budget arm, which flushes it just before the
    /// synthetic `virtualTimeBudgetExpired`. An executor that awaits load
    /// BEFORE arming the budget deadlocks here exactly like it does against
    /// real Chromium (the settle-timeout-on-static regression).
    pub(crate) vt_clock_paused: bool,
    /// The `Page.loadEventFired` frame held while `vt_clock_paused`.
    pub(crate) deferred_load_event: Option<String>,
    /// Fetch-pause fidelity: with `Fetch.enable {requestStage:"Request"}`,
    /// real Chromium pauses the matched DOCUMENT request and the navigation
    /// does NOT proceed — no Network.responseReceived, no Page.loadEventFired
    /// — until the client answers Fetch.continueRequest / failRequest for the
    /// paused requestId. Mirror that: the PageWithTracker document's would-be
    /// events are stashed here and only flushed when the answer arrives, so
    /// an interceptor regression that never answers the document pause hangs
    /// the navigate in e2e exactly like it would against real Chromium
    /// (previously the fake emitted them fire-and-forget and could not catch
    /// that divergence class).
    pub(crate) paused_doc: Option<PausedDoc>,
    /// Under `pauseIfNetworkFetchesPending`, a paused document fetch keeps
    /// virtual time from advancing: a budget armed while the pause is
    /// outstanding must not drain (no flush, no virtualTimeBudgetExpired)
    /// until the fetch gate answers.
    pub(crate) vt_budget_pending_on_pause: bool,
    /// Interaction-fingerprint (capture-policy=fingerprint) e2e hook: a
    /// `__loom_test_dom_mutate__` Runtime.evaluate (modeling a DOM-mutating click)
    /// flips this per-connection flag so a SUBSEQUENT DOM.getDocument returns
    /// content-differing DOM. Lets the e2e prove `dom_after_hash` is content-bearing
    /// (differs from a no-op) yet deterministic across same-seed sessions (the
    /// ephemeral frameIds still vary per call and must be normalized away).
    pub(crate) dom_after_mutated: bool,
    /// Client-side-redirect modeling (see `SettleScript::renavigate_at`): true
    /// while the loaded page has begun a self-initiated top-level navigation
    /// whose new document is held `readyState:"loading"` until the executor
    /// re-arms the virtual-time budget. `renav_href` is the URL the wedged
    /// in-flight document reports until then.
    pub(crate) awaiting_rearm: bool,
    /// See `awaiting_rearm`.
    pub(crate) renav_href: String,
    /// Cross-origin process-swap gate: set when the current `awaiting_rearm`
    /// wedge was triggered by `cross_origin_swap_at` (not `renavigate_at`). While
    /// true, the re-arm handler SUPPRESSES the deferred load-flush + budget-expiry
    /// (the swapped-away renderer's events never reach this stale session), so a
    /// buggy multi-phase executor dead-waits each phase and a correct one returns
    /// a bounded typed `timeout`.
    pub(crate) cross_origin_swap_active: bool,
    /// voice-call-io task 07: per-connection echo buffer for the audio harness
    /// (`LOOM_FAKE_CHROMIUM_AUDIO_ECHO`). Set from the `enqueue` argument, taken on the
    /// first `drain`. See `audio_call_function_on`.
    pub(crate) audio_echo_b64: Option<String>,
}

/// Stamp the page session's id on an outgoing frame.
pub(crate) fn stamp(frame: &mut Value, session_id: &Option<String>) {
    if let Some(sid) = session_id {
        frame["sessionId"] = json!(sid);
    }
}

impl Conn {
    pub(crate) fn new(write: WsWrite, require_session_id: bool) -> Self {
        Self {
            write,
            require_session_id,
            fetch_enabled: false,
            settle_idx: 0,
            vt_clock_paused: false,
            deferred_load_event: None,
            paused_doc: None,
            vt_budget_pending_on_pause: false,
            dom_after_mutated: false,
            awaiting_rearm: false,
            renav_href: String::new(),
            cross_origin_swap_active: false,
            audio_echo_b64: None,
        }
    }

    /// Answer one CDP request: the fault injections first, then the response,
    /// then the events real Chromium would emit after it.
    pub(crate) async fn serve(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
        session_id: &Option<String>,
    ) -> Result<(), Closed> {
        if self.inject_fault(id, method, params, session_id).await {
            return Ok(());
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
            self.settle_idx = 0;
            self.paused_doc = None;
            self.vt_budget_pending_on_pause = false;
            self.awaiting_rearm = false;
            self.renav_href = String::new();
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
                self.settle_probe_response(session_id)
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
            self.fetch_enabled = true;
        } else if method == "Fetch.disable" {
            self.fetch_enabled = false;
        }

        // The budget-carrying arm is handled after the response is sent below.
        if method == "Emulation.setVirtualTimePolicy" && params.get("budget").is_none() {
            self.track_clock_pin(params);
        }

        // voice-call-io task 07: audio harness answers the four nonce'd in-page audio
        // API calls (enqueue/startCapture/stopCapture/drain). Handled here (not in the
        // stateless `canned_response`) because echo mode threads per-connection state.
        let audio_response = if method == "Runtime.callFunctionOn" {
            audio_call_function_on(params, &mut self.audio_echo_b64)
        } else {
            None
        };

        let mut result = if let Some(eval_result) = evaluate_response {
            eval_result
        } else if let Some(audio_result) = audio_response {
            audio_result
        } else {
            canned_response(method, params)
        };
        // Interaction-fingerprint (capture-policy=fingerprint) e2e hook: a
        // `__loom_test_dom_mutate__` Runtime.evaluate (modeling a DOM-mutating
        // click) flips the per-connection flag; a SUBSEQUENT DOM.getDocument then
        // returns content-differing DOM (a content text node, NOT an ephemeral id,
        // so normalization keeps it → the dom_after_hash changes vs a no-op).
        if let Some(expr) = &expression {
            if expr.contains("__loom_test_dom_mutate__") {
                self.dom_after_mutated = true;
            }
        }
        if method == "DOM.getDocument" && self.dom_after_mutated {
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

        if let Some(expr) = &expression {
            self.emit_click_doc_events(expr, session_id).await;
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
        stamp(&mut response, session_id);
        let response_text = response.to_string();
        if self
            .write
            .send(Message::Text(response_text.into()))
            .await
            .is_err()
        {
            return Err(Closed);
        }

        if method == "Page.startScreencast" {
            self.emit_screencast_frames(session_id).await?;
        }
        if method == "Emulation.setVirtualTimePolicy" && params.get("budget").is_some() {
            self.on_budget_arm(session_id).await?;
        }
        if method == "Fetch.continueRequest" || method == "Fetch.failRequest" {
            self.on_fetch_answer(method, params, session_id).await?;
        }
        if is_settle_probe && settle_script().perpetual_inflight > 0 {
            self.emit_perpetual_inflight(session_id).await?;
        }
        if method == "Page.navigate" {
            self.emit_navigate_events(params, &nav_url_pattern, session_id)
                .await;
        }
        Ok(())
    }
}
