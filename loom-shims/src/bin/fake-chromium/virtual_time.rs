//! The virtual-time clock: the pause pin, and what a budget arm releases.

use futures::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{stamp, Closed, Conn};

impl Conn {
    /// A budgetless `Emulation.setVirtualTimePolicy`: track the clock pin (see
    /// `vt_clock_paused`). A budgetless `policy:"pause"` is the inject-time origin
    /// pin; the budget-carrying arm is `on_budget_arm`.
    pub(crate) fn track_clock_pin(&mut self, params: &Value) {
        match params.get("policy").and_then(|p| p.as_str()) {
            // Inject-time origin pin (determinism ON) — clock paused.
            Some("pause") => self.vt_clock_paused = true,
            // Navigate-exit RESUME (`resume_virtual_time`,
            // `build_virtual_time_resume_params` → `{policy:"advance"}`, no
            // budget): real Chromium lets virtual time advance freely again, so
            // the clock un-pauses and the NEXT navigate's load fires promptly.
            // Modeling this is what makes the navigate-degradation regression
            // observable here: WITHOUT the resume-guard fix the executor never
            // sends this advance after a clean drain under `--no-determinism`,
            // so `vt_clock_paused` stays true (set when the budget drained
            // below) and the next navigate's load defers → the +20s/call wedge.
            Some("advance") => self.vt_clock_paused = false,
            _ => {}
        }
    }

    /// A budget-carrying setVirtualTimePolicy advances the clock: first
    /// flush any load event deferred by the pause-pin (real Chromium
    /// completes the held load tasks the moment the budget is granted),
    /// then emit Emulation.virtualTimeBudgetExpired so the action_executor's
    /// budget await (cross-run determinism) completes promptly instead of
    /// waiting out its wall-clock timeout. The budget is treated as
    /// instantly drained (the fake has no real virtual clock), after which
    /// the clock is paused again so the NEXT navigate's load event defers
    /// until ITS budget arm, exactly like a second navigation against real
    /// Chromium (CDP `virtualTimeBudgetExpired` leaves virtual time paused).
    /// This re-pause is what reproduces the navigate-degradation wedge under
    /// `--no-determinism`: the executor must send a budgetless `advance`
    /// (resume) on navigate exit to un-pause before the next navigate, which
    /// the resume-guard fix does (the unfixed `!budget_drained` guard skipped
    /// it after a clean drain, so the clock stayed paused and the next load
    /// deferred for the full budget).
    pub(crate) async fn on_budget_arm(
        &mut self,
        session_id: &Option<String>,
    ) -> Result<(), Closed> {
        if self.paused_doc.is_some() {
            // `pauseIfNetworkFetchesPending`: the paused document fetch
            // keeps virtual time from advancing, so the budget cannot
            // drain (and the held load cannot flush) until the Fetch
            // gate answers. The continueRequest/failRequest handler
            // below emits the budget expiry once the pause resolves.
            self.vt_budget_pending_on_pause = true;
        } else if self.cross_origin_swap_active {
            // Cross-origin process swap: the destination's load event and
            // the virtual-time budget expiry both land on a NEW renderer
            // this stale session cannot observe. Suppress BOTH — do NOT
            // flush `deferred_load_event`, do NOT emit
            // `virtualTimeBudgetExpired`. The re-arm command still gets its
            // ack (sent by the generic response path above), so the
            // executor's arm succeeds but then dead-waits the event that
            // never comes — bounded only by its own deadline. The wedge is
            // never cleared (never-recovering).
            self.vt_clock_paused = true;
        } else {
            if let Some(load_evt) = self.deferred_load_event.take() {
                if self
                    .write
                    .send(Message::Text(load_evt.into()))
                    .await
                    .is_err()
                {
                    return Err(Closed);
                }
                // A re-arm that flushed a renavigation's held load event
                // un-wedges the new document: clear the loading gate and
                // step past the renav tick so the next probe returns the
                // scripted post-redirect (complete) observation.
                if self.awaiting_rearm {
                    self.awaiting_rearm = false;
                    self.settle_idx += 1;
                }
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
            // Real Chromium leaves virtual time PAUSED once a budget drains
            // (until the next policy is set). Model that so a subsequent
            // navigate's load event defers — the executor must explicitly
            // resume (`advance`) to clear it.
            self.vt_clock_paused = true;
        }
        Ok(())
    }
}
