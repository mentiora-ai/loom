// ShimManager — `web.type` fill (the default mode): resolve + focus, run the
// prepare step on the resolved node (`fill_prepare`), then either nothing more
// (a date/time-family input set by value) or one `Input.insertText`, all inside
// one shared deadline, and release the step's CDP objects afterwards.

use super::fill_prepare::{
    fill_prepare_message, parse_fill_prepare, release_fill_objects_message, resolve_node_message,
    FillPrep,
};
use super::helpers::cbor_get;
use super::input_dispatch::insert_text_event;
use super::shim_manager::ShimManager;
use super::trusted_input::InputAck;
use super::types::{FailureClass, FillFailure, InputDispatchOutcome, ShimId};
use loom_core::error::{LoomError, LoomErrorCode};

/// Numbers each `web.type` fill's CDP object group, so releasing one fill's
/// remote objects can never free another in-flight fill's.
static FILL_OBJECT_GROUP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// Ceiling on waiting for the best-effort release of a fill's object group.
const FILL_RELEASE_BUDGET_MS: u64 = 250;

/// One deadline shared by the round-trips of a `web.type` fill that follow
/// selector resolution (prepare, insert, release): each draws what is LEFT of the
/// action's budget after resolution, so their sum stays inside it. (Resolution
/// itself keeps the shared locator path's own floor, as for every input verb.)
/// A budget of `0` keeps its "no deadline" meaning.
struct FillDeadline {
    budget_ms: u64,
    started: std::time::Instant,
}

impl FillDeadline {
    fn start(budget_ms: u64) -> Self {
        Self {
            budget_ms,
            started: std::time::Instant::now(),
        }
    }

    /// True once a real budget is spent: the fill then sends nothing more that
    /// could change the page (a write whose ack we could not wait for would leave
    /// the page changed while the receipt says it timed out).
    fn exhausted(&self) -> bool {
        self.budget_ms != 0 && self.started.elapsed().as_millis() >= u128::from(self.budget_ms)
    }

    /// Budget left for the next round-trip, floored to 1 ms: `0` would mean
    /// "no deadline" to `cdp_send_dispatch` (its recv-floor dead-wait).
    fn remaining_ms(&self) -> u64 {
        if self.budget_ms == 0 {
            return 0;
        }
        let spent = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.budget_ms.saturating_sub(spent).max(1)
    }
}

impl ShimManager {
    /// `web.type` DEFAULT (`mode:"fill"`) — Playwright `fill()` semantics, on the
    /// node `selector` RESOLVED to (never a re-query of the raw locator, which
    /// `document.querySelector` cannot parse for `role=`/`text=`/`css=`/`frame=`):
    /// focus it, then [`fill_resolved_node`](Self::fill_resolved_node) either sets a
    /// date/time-family input by value (Chromium ignores `Input.insertText` on
    /// those) or selects the existing content, which the one GENUINE
    /// (`isTrusted:true`) `Input.insertText` then replaces — so React /
    /// react-hook-form `onChange` fires and the value is treated as user-entered.
    /// A disabled/readonly target, a value the input rejects, or a page-side reason
    /// the element cannot be filled is a typed outcome, never a success with the
    /// field unchanged. Only a transport failure (incl. a lost prepare ack) is an
    /// `Err` that counts against the shim's breaker.
    pub async fn send_type_fill(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        text: String,
        budget_ms: u64,
    ) -> Result<InputDispatchOutcome, LoomError> {
        self.check_breaker(&id)?;
        let deadline = FillDeadline::start(budget_ms);
        let Some(node_id) = self
            .resolve_and_focus(&id, session_id, target_id, &selector, budget_ms)
            .await
            .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
        else {
            self.record_success(&id);
            return Ok(InputDispatchOutcome::SelectorNotFound);
        };
        let group = format!(
            "loom-fill-{}",
            FILL_OBJECT_GROUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let outcome = self
            .fill_resolved_node(
                &id, session_id, target_id, node_id, &text, &group, &deadline,
            )
            .await;
        self.release_fill_objects(&id, session_id, target_id, &group, &deadline)
            .await;
        match &outcome {
            Ok(_) => self.record_success(&id),
            Err(_) => self.record_failure(&id, FailureClass::Application),
        }
        outcome
    }

    /// Run [`fill_prepare_fn`] on the resolved node (`DOM.resolveNode` →
    /// `Runtime.callFunctionOn`, the typed text travelling as its argument) and act
    /// on its verdict. A lost ack before the verdict arrives is an `Err`, not a
    /// dispatched input: the insert path has not typed anything yet and a
    /// set-value input may or may not have taken the value.
    #[allow(clippy::too_many_arguments)]
    async fn fill_resolved_node(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        node_id: u64,
        text: &str,
        group: &str,
        deadline: &FillDeadline,
    ) -> Result<InputDispatchOutcome, LoomError> {
        use ciborium::value::Value;
        let unacknowledged = || {
            LoomError::new(
                LoomErrorCode::ShimTimeout,
                format!(
                    "shim {}: web.type fill: prepare step unacknowledged — field state unknown",
                    id.0
                ),
            )
        };
        // The budget ran out before the next write: nothing more is sent, so the
        // field is exactly as the last acknowledged step left it (never half-typed).
        let out_of_time = || {
            LoomError::new(
                LoomErrorCode::ShimTimeout,
                format!(
                    "shim {}: web.type fill: the action's deadline ran out before the text was written",
                    id.0
                ),
            )
        };
        // Only THAT the page side failed is logged — never its own text (a CDP
        // error about its node, an exception message), which can echo the typed value.
        let failed = |failure: FillFailure| {
            tracing::debug!(shim = %id.0, ?failure, "web.type fill could not fill the element");
            Ok(InputDispatchOutcome::FillFailed(failure))
        };

        if deadline.exhausted() {
            return Err(out_of_time());
        }
        let resolved = match self
            .cdp_send_dispatch(
                id,
                session_id,
                target_id,
                resolve_node_message(node_id, group),
                deadline.remaining_ms(),
            )
            .await?
        {
            None => return Err(unacknowledged()),
            Some(Err(_)) => return failed(FillFailure::Rejected),
            Some(Ok(resolved)) => resolved,
        };
        let object_id = match cbor_get(&resolved, "object").and_then(|o| cbor_get(o, "objectId")) {
            Some(Value::Text(object_id)) => object_id.clone(),
            _ => return failed(FillFailure::NoObject),
        };
        if deadline.exhausted() {
            return Err(out_of_time());
        }
        let verdict = match self
            .cdp_send_dispatch(
                id,
                session_id,
                target_id,
                fill_prepare_message(&object_id, text),
                deadline.remaining_ms(),
            )
            .await?
        {
            None => return Err(unacknowledged()),
            Some(Err(_)) => return failed(FillFailure::Rejected),
            Some(Ok(payload)) => parse_fill_prepare(&payload),
        };
        match verdict {
            Ok(FillPrep::ValueSet) => Ok(InputDispatchOutcome::Ok),
            Ok(FillPrep::Malformed(input_type)) => {
                Ok(InputDispatchOutcome::MalformedValue(input_type))
            }
            Ok(FillPrep::NotEditable) => Ok(InputDispatchOutcome::NotEditable),
            Ok(FillPrep::Insert) if deadline.exhausted() => Err(out_of_time()),
            Ok(FillPrep::Insert) => self
                .dispatch_input_events(
                    id,
                    session_id,
                    target_id,
                    vec![insert_text_event(text)],
                    deadline.remaining_ms(),
                    0,
                )
                .await
                .map(InputAck::into_outcome),
            Err(failure) => failed(failure),
        }
    }

    /// Release a fill's CDP object group, AFTER the fill committed. Best-effort and
    /// briefly bounded: its failure or lost ack never changes the fill's outcome.
    async fn release_fill_objects(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        group: &str,
        deadline: &FillDeadline,
    ) {
        let budget_ms = match deadline.remaining_ms() {
            0 => FILL_RELEASE_BUDGET_MS,
            remaining => remaining.min(FILL_RELEASE_BUDGET_MS),
        };
        let released = self
            .cdp_send_dispatch(
                id,
                session_id,
                target_id,
                release_fill_objects_message(group),
                budget_ms,
            )
            .await;
        if !matches!(released, Ok(Some(Ok(_)))) {
            tracing::debug!(shim = %id.0, group, "web.type fill: object group release did not complete");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_deadline_draws_down_one_budget() {
        let unbounded = FillDeadline::start(0);
        assert_eq!(
            unbounded.remaining_ms(),
            0,
            "0 keeps its no-deadline meaning"
        );
        assert!(!unbounded.exhausted());

        let roomy = FillDeadline::start(60_000);
        assert!(!roomy.exhausted());
        let left = roomy.remaining_ms();
        assert!(left > 59_000 && left <= 60_000, "{left}");

        let spent = FillDeadline {
            budget_ms: 5,
            started: std::time::Instant::now() - std::time::Duration::from_millis(50),
        };
        assert!(spent.exhausted());
        assert_eq!(
            spent.remaining_ms(),
            1,
            "floored to 1 ms, never 0 (= no deadline)"
        );
    }
}
