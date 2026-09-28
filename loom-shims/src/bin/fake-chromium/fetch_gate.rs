//! The Fetch gate: a document request paused until the client answers it.

use futures::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{stamp, Closed, Conn};

/// A document request held at the Fetch gate (PageWithTracker with
/// `Fetch.enable` issued). Carries the would-be navigation events until
/// the client answers `Fetch.continueRequest` (flush) or
/// `Fetch.failRequest` (abort with `Network.loadingFailed`).
pub(crate) struct PausedDoc {
    /// The paused Fetch requestId the client must answer.
    pub(crate) request_id: String,
    /// Held event frames (the document's `Network.responseReceived`).
    pub(crate) held_events: Vec<String>,
    /// The held `Page.loadEventFired` frame, populated when the navigate
    /// block reaches its load-event emission.
    pub(crate) load_event: Option<String>,
}

impl Conn {
    /// Answering the document's Fetch pause releases the held
    /// navigation: continueRequest flushes the document response +
    /// load event (real Chromium resumes the request); failRequest
    /// aborts it with a Document loadingFailed and NO load event.
    pub(crate) async fn on_fetch_answer(
        &mut self,
        method: &str,
        params: &Value,
        session_id: &Option<String>,
    ) -> Result<(), Closed> {
        let answered_id = params
            .get("requestId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if self
            .paused_doc
            .as_ref()
            .is_some_and(|pd| pd.request_id == answered_id)
        {
            let pd = self.paused_doc.take().expect("checked is_some above");
            if method == "Fetch.continueRequest" {
                for held in pd.held_events {
                    if self.write.send(Message::Text(held.into())).await.is_err() {
                        return Err(Closed);
                    }
                }
                if let Some(load_evt) = pd.load_event {
                    if self.vt_budget_pending_on_pause {
                        // The armed budget resumes draining now that
                        // the fetch gate is clear: the held load
                        // completes, then the budget expires.
                        self.vt_budget_pending_on_pause = false;
                        if self
                            .write
                            .send(Message::Text(load_evt.into()))
                            .await
                            .is_err()
                        {
                            return Err(Closed);
                        }
                        let mut vt_evt = json!({
                            "method": "Emulation.virtualTimeBudgetExpired",
                            "params": {},
                        });
                        stamp(&mut vt_evt, session_id);
                        if self
                            .write
                            .send(Message::Text(vt_evt.to_string().into()))
                            .await
                            .is_err()
                        {
                            return Err(Closed);
                        }
                    } else if self.vt_clock_paused {
                        self.deferred_load_event = Some(load_evt);
                    } else if self
                        .write
                        .send(Message::Text(load_evt.into()))
                        .await
                        .is_err()
                    {
                        return Err(Closed);
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
                stamp(&mut fail_evt, session_id);
                if self
                    .write
                    .send(Message::Text(fail_evt.to_string().into()))
                    .await
                    .is_err()
                {
                    return Err(Closed);
                }
                if self.vt_budget_pending_on_pause {
                    // The aborted fetch no longer pends; the budget
                    // drains with nothing further to flush.
                    self.vt_budget_pending_on_pause = false;
                    let mut vt_evt = json!({
                        "method": "Emulation.virtualTimeBudgetExpired",
                        "params": {},
                    });
                    stamp(&mut vt_evt, session_id);
                    if self
                        .write
                        .send(Message::Text(vt_evt.to_string().into()))
                        .await
                        .is_err()
                    {
                        return Err(Closed);
                    }
                }
            }
        }
        Ok(())
    }
}
