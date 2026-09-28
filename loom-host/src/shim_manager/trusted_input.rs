// ShimManager — trusted CDP input: focus a resolved element, then dispatch real
// `Input.*` frames (`web.type mode:"keystrokes"`, `web.press_key`, the always-
// trusted `web.click`) and classify their acks.

use super::helpers::{cbor_get, map_shim_code};
use super::input_dispatch::{
    dispatch_frame_step, keystroke_events_for_text, mouse_event, press_key_events, FrameAck,
    FrameStep,
};
use super::shim_manager::ShimManager;
use super::types::{FailureClass, InputDispatchOutcome, SendPressKeyParams, ShimId};
use loom_core::error::LoomError;
use loom_shared::shim_protocol::CdpMessage;

/// Result of [`ShimManager::dispatch_input_events`]: all input frames were acked
/// (`Acked`), or the committing frame was written but its ack was lost to a
/// likely cross-origin renderer swap (`Dispatched`). Maps to the trusted-input
/// verb's `InputDispatchOutcome`.
pub(super) enum InputAck {
    Acked,
    Dispatched,
}

impl InputAck {
    pub(super) fn into_outcome(self) -> InputDispatchOutcome {
        match self {
            InputAck::Acked => InputDispatchOutcome::Ok,
            InputAck::Dispatched => InputDispatchOutcome::DispatchedAckPending,
        }
    }
}

impl ShimManager {
    /// Record a trusted-input dispatch against the shim's breaker and map its ack
    /// to the verb's outcome: an ack (clean, or lost to a renderer swap) is a
    /// success; an error counts as an application failure.
    fn settle_dispatch(
        &self,
        id: &ShimId,
        dispatched: Result<InputAck, LoomError>,
    ) -> Result<InputDispatchOutcome, LoomError> {
        match dispatched {
            Ok(ack) => {
                self.record_success(id);
                Ok(ack.into_outcome())
            }
            Err(e) => {
                self.record_failure(id, FailureClass::Application);
                Err(e)
            }
        }
    }

    /// Resolve `selector` to a node and focus it. `Ok(Some(nodeId))` focused;
    /// `Ok(None)` selector matched nothing; `Err` on transport failure. Focus
    /// itself is best-effort (a non-focusable node still receives dispatched key
    /// events). The nodeId is only valid until the next `DOM.getDocument`, so a
    /// caller acts on it straight away.
    pub(super) async fn resolve_and_focus(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        selector: &str,
        budget_ms: u64,
    ) -> Result<Option<u64>, LoomError> {
        use ciborium::value::{Integer, Value};
        // Frame-aware resolution (descends same-process cross-origin iframes);
        // for a bare/plain CSS selector this is the same getDocument →
        // querySelector path as before.
        let node = match self
            .resolve_locator_node(id, session_id, target_id, selector, budget_ms)
            .await?
        {
            Some(n) => n,
            None => return Ok(None),
        };
        // Best-effort focus — ignore a CDP error (non-focusable element).
        // `Input.insertText`/`dispatchKeyEvent` then target the focused element,
        // which is correct even when it lives inside a cross-origin frame.
        let _ = self
            .cdp_send_one(
                id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.focus".into(),
                    params: Value::Map(vec![(
                        Value::Text("nodeId".into()),
                        Value::Integer(Integer::from(node)),
                    )]),
                },
                budget_ms,
            )
            .await?;
        Ok(Some(node))
    }

    /// Dispatch a prebuilt sequence of `Input.*` frames. The per-frame ack decision
    /// is the pure [`dispatch_frame_step`] classifier (exhaustively unit-tested):
    /// a CDP app error aborts (`ReturnError`); a recv timeout on the frame at or
    /// after `commit_from_index` — the COMMITTING input (`mousePressed` for a click;
    /// index 0 for a key/text frame) — returns [`InputAck::Dispatched`] (the input
    /// was written but its ack was lost, the cross-origin process-swap case).
    ///
    /// A recv timeout BEFORE the committing frame (e.g. `mouseMoved`) no longer
    /// aborts eagerly: a cross-origin swap can open at/before the move and swallow
    /// that ack too, so we KEEP DISPATCHING the committing frames (the click is
    /// genuinely written) rather than mislabel a performed-but-swapped click a
    /// transport `shim_timeout`. If the committing frames instead ACK, the session
    /// is alive and the click committed cleanly, so a dropped `mouseMoved` ack ALONE
    /// does not degrade the outcome → `Acked`.
    ///
    /// **Wall-clock bounding.** Each frame is bounded by the per-frame `budget_ms`
    /// (unchanged from v0.15.2 — every caller, incl. the keystroke / fill / press-key
    /// paths, keeps identical timeout semantics). Tolerating the pre-commit frame
    /// adds AT MOST one extra timed-out frame (the `mouseMoved`) before the committing
    /// frame, so a swapped click's dispatch is bounded by ~2× `budget_ms` in the worst
    /// case — still bounded, never a dead-wait, and it fits inside the daemon's
    /// elapsed-aware settle budget under the per-call deadline. `budget_ms == 0` (a
    /// direct in-crate caller with no deadline) keeps the `config.recv_timeout_ms`
    /// fallback in `cdp_send_dispatch`.
    pub(super) async fn dispatch_input_events(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        events: Vec<CdpMessage>,
        budget_ms: u64,
        commit_from_index: usize,
    ) -> Result<InputAck, LoomError> {
        debug_assert!(
            commit_from_index == 0 || commit_from_index < events.len(),
            "dispatch_input_events: commit_from_index {commit_from_index} out of range for {} frames \
             (a committing frame must be reachable, else a swapped click can't be reported dispatched)",
            events.len()
        );
        for (i, ev) in events.into_iter().enumerate() {
            let (ack, app_err) = match self
                .cdp_send_dispatch(id, session_id, target_id, ev, budget_ms)
                .await?
            {
                Some(Ok(_)) => (FrameAck::Acked, None),
                Some(Err((code, detail))) => (FrameAck::AppError, Some((code, detail))),
                None => (FrameAck::AckLost, None),
            };
            match dispatch_frame_step(i, commit_from_index, ack) {
                FrameStep::Advance => {
                    // The only non-clean advance is a tolerated pre-commit ack loss;
                    // log it (off-chain) so on-call can correlate a later swap.
                    if ack == FrameAck::AckLost {
                        tracing::debug!(
                            shim = %id.0,
                            target_id,
                            session_id,
                            frame_index = i,
                            commit_from_index,
                            "trusted-input pre-commit ack lost — continuing to the committing frames"
                        );
                    }
                }
                // The committing input was written but its ack was lost — the
                // cross-origin process-swap case. Off-chain debug signal so on-call
                // can correlate a `dispatched` click whose destination later fails to
                // settle (the primary production signal is the receipt's
                // `DispatchedAckPending` outcome + `settle_outcome`; the loud
                // `shim_timeout` this replaces is gone).
                FrameStep::ReturnDispatched => {
                    tracing::debug!(
                        shim = %id.0,
                        target_id,
                        session_id,
                        frame_index = i,
                        commit_from_index,
                        "trusted-input committing-frame ack lost (likely cross-origin \
                         renderer swap) — reporting dispatched"
                    );
                    return Ok(InputAck::Dispatched);
                }
                FrameStep::ReturnError => {
                    let (code, detail) =
                        app_err.expect("AppError frame ack carries the CDP error detail");
                    return Err(LoomError::new(
                        map_shim_code(code),
                        format!("shim {}: input dispatch: {detail}", id.0),
                    ));
                }
            }
        }
        // Every committing frame ACKed (a committing-frame timeout early-returns
        // `Dispatched`). The click committed and its commit ack arrived, so this is
        // a clean success even if an earlier `mouseMoved` ack was dropped.
        Ok(InputAck::Acked)
    }

    /// `web.type mode:keystrokes` — focus `selector`, then send a real per-char
    /// `Input.dispatchKeyEvent` (keyDown+text → keyUp) sequence.
    pub async fn send_type_keystrokes(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        text: String,
        budget_ms: u64,
    ) -> Result<InputDispatchOutcome, LoomError> {
        self.check_breaker(&id)?;
        if self
            .resolve_and_focus(&id, session_id, target_id, &selector, budget_ms)
            .await
            .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
            .is_none()
        {
            self.record_success(&id);
            return Ok(InputDispatchOutcome::SelectorNotFound);
        }
        let dispatched = self
            .dispatch_input_events(
                &id,
                session_id,
                target_id,
                keystroke_events_for_text(&text),
                budget_ms,
                0,
            )
            .await;
        self.settle_dispatch(&id, dispatched)
    }

    /// `web.press_key` — optionally focus `selector`, then dispatch a named key
    /// (+ modifier combo) as real `Input.dispatchKeyEvent` frames. An unknown
    /// key / modifier is the typed `UnknownKey` outcome (not a transport error).
    pub async fn send_press_key(
        &self,
        params: SendPressKeyParams,
    ) -> Result<InputDispatchOutcome, LoomError> {
        let SendPressKeyParams {
            id,
            session_id,
            target_id,
            key,
            selector,
            modifiers,
            budget_ms,
        } = params;
        self.check_breaker(&id)?;
        if let Some(sel) = &selector {
            if self
                .resolve_and_focus(&id, session_id, target_id, sel, budget_ms)
                .await
                .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
                .is_none()
            {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::SelectorNotFound);
            }
        }
        let events = match press_key_events(&key, &modifiers) {
            Some(e) => e,
            None => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::UnknownKey);
            }
        };
        let dispatched = self
            .dispatch_input_events(&id, session_id, target_id, events, budget_ms, 0)
            .await;
        self.settle_dispatch(&id, dispatched)
    }

    /// `Input.dispatchMouseEvent` mouseMoved→mousePressed→mouseReleased. No
    /// `el.click()` fallback. `SelectorNotFound` / `NotHittable` are typed
    /// outcomes; transport failures surface as `Err`.
    pub async fn send_trusted_click(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        budget_ms: u64,
    ) -> Result<InputDispatchOutcome, LoomError> {
        use ciborium::value::{Integer, Value};
        self.check_breaker(&id)?;

        // Resolve the (possibly `frame=`-prefixed) locator to a node id,
        // descending into same-process iframes for cross-origin reach.
        let node = match self
            .resolve_locator_node(&id, session_id, target_id, &selector, budget_ms)
            .await
            .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
        {
            Some(n) => n,
            None => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::SelectorNotFound);
            }
        };

        // Scroll into view (best-effort) before resolving coordinates.
        let _ = self
            .cdp_send_one(
                &id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.scrollIntoViewIfNeeded".into(),
                    params: Value::Map(vec![(
                        Value::Text("nodeId".into()),
                        Value::Integer(Integer::from(node)),
                    )]),
                },
                budget_ms,
            )
            .await?;

        // Box model → content-quad center. A CDP error here means the element
        // has no box model (display:none / detached) → NotHittable.
        let box_payload = match self
            .cdp_send_one(
                &id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.getBoxModel".into(),
                    params: Value::Map(vec![(
                        Value::Text("nodeId".into()),
                        Value::Integer(Integer::from(node)),
                    )]),
                },
                budget_ms,
            )
            .await?
        {
            Ok(p) => p,
            Err(_) => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::NotHittable);
            }
        };
        let (cx, cy) = match content_quad_center(&box_payload) {
            Some(c) => c,
            None => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::NotHittable);
            }
        };

        // Trusted click: mouseMoved → mousePressed → mouseReleased at (cx, cy).
        let events = vec![
            mouse_event("mouseMoved", cx, cy, "none", 0),
            mouse_event("mousePressed", cx, cy, "left", 1),
            mouse_event("mouseReleased", cx, cy, "left", 1),
        ];
        // commit_from_index = 1: `mousePressed` (index 1, after `mouseMoved`) is
        // the event that commits the click + can trigger the navigation whose
        // swap swallows the ack. A recv timeout there ⇒ the click WAS performed
        // (`DispatchedAckPending`). A pre-commit `mouseMoved` (index 0) timeout no
        // longer aborts: a swap that opens at/before the move swallows that ack
        // too, so `dispatch_input_events` keeps dispatching the committing frames
        // and reports the performed-but-swapped click as `DispatchedAckPending`.
        let dispatched = self
            .dispatch_input_events(&id, session_id, target_id, events, budget_ms, 1)
            .await;
        self.settle_dispatch(&id, dispatched)
    }
}

/// Center of a CDP `DOM.getBoxModel` content quad. `content` is
/// `[x1,y1,x2,y2,x3,y3,x4,y4]`; center = midpoint of opposite corners
/// (1 and 3). `None` when the payload lacks a usable quad.
fn content_quad_center(payload: &ciborium::value::Value) -> Option<(i64, i64)> {
    use ciborium::value::Value;
    let num = |v: &Value| -> Option<f64> {
        match v {
            Value::Float(f) => Some(*f),
            Value::Integer(i) => i64::try_from(*i).ok().map(|n| n as f64),
            _ => None,
        }
    };
    let content = cbor_get(payload, "model").and_then(|m| cbor_get(m, "content"))?;
    if let Value::Array(pts) = content {
        if pts.len() >= 6 {
            let x1 = num(&pts[0])?;
            let y1 = num(&pts[1])?;
            let x3 = num(&pts[4])?;
            let y3 = num(&pts[5])?;
            return Some((
                ((x1 + x3) / 2.0).round() as i64,
                ((y1 + y3) / 2.0).round() as i64,
            ));
        }
    }
    None
}
