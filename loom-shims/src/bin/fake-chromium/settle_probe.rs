//! The settle-capture readiness probe and its scripted network noise.

use futures::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{stamp, Closed, Conn};
use crate::settle_script::settle_script;

impl Conn {
    /// The response to the settle probe (the `Runtime.evaluate` carrying the
    /// `__loomSettleMut` global). It advances per-connection script state.
    pub(crate) fn settle_probe_response(&mut self, session_id: &Option<String>) -> Option<Value> {
        // Client-side-redirect gate: if the page is scripted to begin a
        // self-initiated top-level navigation at this tick, queue a NEW
        // load event held on the next budget arm and pin the probe to
        // "loading" until the executor re-arms (see `renavigate_at`).
        // A `cross_origin_swap_at` tick enters the SAME wedge but marks
        // `cross_origin_swap_active` so the re-arm handler suppresses the
        // flush + expiry (the swapped renderer's events never reach this
        // stale session).
        let swap_tick = settle_script()
            .cross_origin_swap_at
            .contains(&self.settle_idx);
        if !self.awaiting_rearm
            && (settle_script().renavigate_at.contains(&self.settle_idx) || swap_tick)
        {
            self.awaiting_rearm = true;
            self.cross_origin_swap_active = swap_tick;
            self.renav_href = settle_script()
                .probe
                .get(self.settle_idx)
                .map(|(_, h, _)| h.clone())
                .unwrap_or_default();
            let mut evt = json!({
                "method": "Page.loadEventFired",
                "params": { "timestamp": 1.0 }
            });
            stamp(&mut evt, session_id);
            // The new document's load is held until the clock advances,
            // exactly like the shell's was. The clock is already paused
            // again (it re-pauses after each budget drains). For a
            // cross-origin swap this held event is NEVER flushed (see the
            // re-arm handler) — it models the load landing on a renderer
            // the stale session cannot see.
            self.deferred_load_event = Some(evt.to_string());
            self.vt_clock_paused = true;
        }
        if self.awaiting_rearm {
            if self.cross_origin_swap_active {
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
                let encoded = json!([false, self.renav_href, 0]).to_string();
                Some(json!({ "result": { "type": "string", "value": encoded } }))
            }
        } else {
            let resp = settle_script().probe_response(self.settle_idx);
            self.settle_idx += 1;
            Some(resp)
        }
    }

    /// settle-capture never-settles (network) shape: re-assert N
    /// never-finishing in-flight requests on every settle probe. Stable
    /// requestIds make the inserts idempotent in the host's in-flight set,
    /// so the count stays pinned at N regardless of when the host's
    /// `Network.` handler registered (it registers only once the settle
    /// wait begins, after this navigate's load fires). N > the idle
    /// threshold keeps `networkidle` from ever quiescing → bounded Timeout.
    pub(crate) async fn emit_perpetual_inflight(
        &mut self,
        session_id: &Option<String>,
    ) -> Result<(), Closed> {
        for i in 0..settle_script().perpetual_inflight {
            let mut evt = json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": format!("perpetual-{i}"),
                    "type": "Fetch",
                    "request": { "url": "http://fake.test/poll", "method": "GET" },
                },
            });
            stamp(&mut evt, session_id);
            if self
                .write
                .send(Message::Text(evt.to_string().into()))
                .await
                .is_err()
            {
                return Err(Closed);
            }
        }
        Ok(())
    }
}
