//! Fault injection ahead of the response: withheld, delayed or failed acks.

use futures::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::canned::is_browser_scope_method_local;
use crate::conn::{stamp, Conn};
use crate::dom_fixture::{dom_fixture, fill_prepare_ack_swallowed};
use crate::settle_script::settle_script;

impl Conn {
    /// Returns `true` when the request was consumed here (answered with an error,
    /// or its ack withheld) and must not get the normal response.
    pub(crate) async fn inject_fault(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
        session_id: &Option<String>,
    ) -> bool {
        // Enforcement: page-scope methods MUST carry sessionId.
        if self.require_session_id && session_id.is_none() && !is_browser_scope_method_local(method)
        {
            let err = json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("'{method}' wasn't found"),
                },
            });
            let _ = self.write.send(Message::Text(err.to_string().into())).await;
            return true;
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
            return true;
        }

        // Cross-origin swap whose window opens at/before the move: the stale CDP
        // session goes blind before even the PRE-COMMIT (`mouseMoved`) ack, so
        // withhold the ack for EVERY trusted-click mouse frame. This exercises the
        // surviving "input dispatch ack timeout before commit" arm that
        // `swallow_dispatch_ack` (commit-frame only) cannot reach.
        if settle_script().swallow_dispatch_ack_from_move && method == "Input.dispatchMouseEvent" {
            return true;
        }

        // web.type fill prepare step losing its ack (fixture `inputs` verdicts
        // `swallow_resolve` / `swallow_call`): the host must bound the wait by its
        // budget and report an error — never a success, never the 30 s recv floor.
        if fill_prepare_ack_swallowed(method, params) {
            return true;
        }
        // Fixture `slow_focus_ms`: answer `DOM.focus` late, so a caller's budget is
        // spent by selector resolution whatever the host's speed.
        if method == "DOM.focus" && dom_fixture().slow_focus_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(
                dom_fixture().slow_focus_ms,
            ))
            .await;
        }

        // Withhold ONLY the PRE-COMMIT (`mouseMoved`) ack; the committing frames
        // still ack. Models a transient move-ack drop on a LIVE session — the click
        // still commits cleanly, so the host must return `Ok` (a dropped move ack
        // alone must NOT degrade a fully-committed click to `DispatchedAckPending`).
        if settle_script().swallow_dispatch_ack_move_only
            && method == "Input.dispatchMouseEvent"
            && params.get("type").and_then(|t| t.as_str()) == Some("mouseMoved")
        {
            return true;
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
            stamp(&mut err, session_id);
            let _ = self.write.send(Message::Text(err.to_string().into())).await;
            return true;
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
        false
    }
}
