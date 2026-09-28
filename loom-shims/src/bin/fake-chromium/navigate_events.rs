//! The network and load events real Chromium emits around a navigation.

use futures::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{stamp, Conn};
use crate::fetch_gate::PausedDoc;
use crate::url_pattern::FakeUrlPattern;

impl Conn {
    /// Per-URL synthetic CDP events emitted right after Page.navigate
    /// resolves. Order matches what real Chromium produces:
    /// Network.* events fire before Page.loadEventFired.
    pub(crate) async fn emit_navigate_events(
        &mut self,
        params: &Value,
        nav_url_pattern: &FakeUrlPattern,
        session_id: &Option<String>,
    ) {
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
                stamp(&mut doc_req, session_id);
                let _ = self
                    .write
                    .send(Message::Text(doc_req.to_string().into()))
                    .await;

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
                stamp(&mut evt, session_id);
                let _ = self.write.send(Message::Text(evt.to_string().into())).await;

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
                stamp(&mut doc_finished, session_id);
                let _ = self
                    .write
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
                stamp(&mut xhr_req, session_id);
                let _ = self
                    .write
                    .send(Message::Text(xhr_req.to_string().into()))
                    .await;

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
                stamp(&mut xhr_resp, session_id);
                let _ = self
                    .write
                    .send(Message::Text(xhr_resp.to_string().into()))
                    .await;
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
                stamp(&mut evt, session_id);
                let _ = self.write.send(Message::Text(evt.to_string().into())).await;
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
                if self.fetch_enabled {
                    let mut doc_evt = json!({
                        "method": "Fetch.requestPaused",
                        "params": {
                            "requestId": "fake-fetch-doc-1",
                            "request": { "url": &nav_url, "method": "GET" },
                            "frameId": "fake-frame-1",
                            "resourceType": "Document"
                        }
                    });
                    stamp(&mut doc_evt, session_id);
                    let _ = self
                        .write
                        .send(Message::Text(doc_evt.to_string().into()))
                        .await;

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
                    stamp(&mut ga_evt, session_id);
                    let _ = self
                        .write
                        .send(Message::Text(ga_evt.to_string().into()))
                        .await;
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
                stamp(&mut resp_evt, session_id);
                if self.fetch_enabled {
                    self.paused_doc = Some(PausedDoc {
                        request_id: "fake-fetch-doc-1".to_string(),
                        held_events: vec![resp_evt.to_string()],
                        load_event: None,
                    });
                } else {
                    let _ = self
                        .write
                        .send(Message::Text(resp_evt.to_string().into()))
                        .await;
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
                stamp(&mut main_req, session_id);
                let _ = self
                    .write
                    .send(Message::Text(main_req.to_string().into()))
                    .await;

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
                stamp(&mut main_resp, session_id);
                let _ = self
                    .write
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
                stamp(&mut iframe_req, session_id);
                let _ = self
                    .write
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
                stamp(&mut iframe_resp, session_id);
                let _ = self
                    .write
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
        stamp(&mut evt, session_id);
        if let Some(pd) = self.paused_doc.as_mut() {
            pd.load_event = Some(evt.to_string());
        } else if self.vt_clock_paused {
            self.deferred_load_event = Some(evt.to_string());
        } else {
            let _ = self.write.send(Message::Text(evt.to_string().into())).await;
        }
    }

    /// Stale-event injection sentinel for Runtime.evaluate
    /// (`__loom_test_emit_doc_event__:<status>`): emit a Document
    /// requestWillBeSent + responseReceived BEFORE the evaluate
    /// response, modeling an in-session CLICK that triggered a real
    /// link navigation between two navigates. Those Document events
    /// accumulate in the shim's hashed path with NO drain until the
    /// next navigate — which must discard them at its START
    /// (`clear_events`) instead of letting them poison its
    /// status_code / network_events. Distinct loaderId so loader
    /// matching can also tell it apart from a current navigation.
    pub(crate) async fn emit_click_doc_events(&mut self, expr: &str, session_id: &Option<String>) {
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
                stamp(&mut click_req, session_id);
                let _ = self
                    .write
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
                stamp(&mut click_resp, session_id);
                let _ = self
                    .write
                    .send(Message::Text(click_resp.to_string().into()))
                    .await;
            }
        }
    }
}
