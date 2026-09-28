// Settle machinery shared by navigate and `wait_for`: virtual-time resume,
// re-arming across a cross-process reattach, the bounded settle loop, and the
// `wait_for` body (the trait method delegates here). Moved verbatim.

use crate::action_executor::action_executor::navigate_budget;
use crate::action_executor::action_executor::ActionResult;
use crate::action_executor::action_executor::ChromiumActionExecutor;
use crate::action_executor::action_executor::MAX_REATTACH_HOPS;
use crate::action_executor::action_executor::MAX_SETTLE_TIMEOUT;
use crate::action_executor::action_executor::RESUME_CDP_TIMEOUT;
use crate::cdp_connection::cdp_connection::{
    CdpConnection, EventFilter, EventHandler, EventRegistration,
};
use crate::ipc_endpoint::ipc_endpoint::{CdpMessage, ShimResponse, TargetId};
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Which command is driving the shared client-nav re-attach loop. Selects the
/// log verb and the exhausted-chain tail so `page_navigate` and `wait_for`
/// stay distinguishable in traces (the messages are otherwise identical).
#[derive(Clone, Copy)]
pub(crate) enum ReattachKind {
    Navigate,
    WaitFor,
}

impl ReattachKind {
    fn verb(self) -> &'static str {
        match self {
            ReattachKind::Navigate => "navigate",
            ReattachKind::WaitFor => "wait_for",
        }
    }

    /// The clause appended to the hop-cap/deadline-exhausted warning — navigate
    /// captures the current document, wait_for resolves on it.
    fn exhausted_tail(self) -> &'static str {
        match self {
            ReattachKind::Navigate => "capturing current document",
            ReattachKind::WaitFor => "resolving on current document",
        }
    }
}

/// Result of driving the bounded client-nav re-attach chain shared by
/// `page_navigate` and `wait_for`. `settle` is the final settle verdict to
/// capture/report on; `last_drained` is `Some(budget_drained)` from the final
/// determinism-clock re-arm hop, or `None` when no vt-arm hop ran (no
/// renavigation, or virtual time disabled) — letting each caller apply its own
/// post-loop virtual-time bookkeeping without the helper knowing about it.
pub(crate) struct ReattachOutcome {
    pub(crate) settle: crate::readiness_monitor::SettleResult,
    pub(crate) last_drained: Option<bool>,
}

impl ChromiumActionExecutor {
    /// Register a one-shot listener for a CDP event and return the receiver
    /// PLUS its RAII registration token. The handler fires the channel the
    /// first time `method` is observed. Register BEFORE issuing the command
    /// that triggers the event (fast events — cached/data: URL loads, an
    /// instantly-drained virtual-time budget — can otherwise fire before a
    /// post-command subscription lands). The caller must keep the token alive
    /// while awaiting the receiver: dropping it DEREGISTERS the handler, so
    /// per-navigate subscriptions no longer accumulate on the connection for
    /// the session's lifetime.
    pub(crate) fn subscribe_cdp_event_once(
        &self,
        method: &'static str,
    ) -> (oneshot::Receiver<()>, EventRegistration) {
        let signal: Arc<parking_lot::Mutex<Option<oneshot::Sender<()>>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let (tx, rx) = oneshot::channel::<()>();
        *signal.lock() = Some(tx);
        let signal_for_handler = signal.clone();
        let handler: EventHandler = Arc::new(move |_target_id, msg: CdpMessage| {
            if msg.method == method {
                if let Some(tx) = signal_for_handler.lock().take() {
                    let _ = tx.send(());
                }
            }
        });
        let reg = self
            .cdp
            .register_event_handler(EventFilter::new(method), handler);
        (rx, reg)
    }

    /// animation-capture Mode A: best-effort RESUME of virtual time after a
    /// navigate. Issues `setVirtualTimePolicy{advance}` so a renderer left paused
    /// at the (possibly half-drained) budget horizon commits frames again and the
    /// NEXT standalone command (e.g. `Page.captureScreenshot`) is not wedged for
    /// the full 30s CDP timeout. Errors are swallowed — this is a cleanup guard,
    /// not a failure path — and it uses the caller's timeout so it can never wedge.
    pub(crate) async fn resume_virtual_time(&self, target_id: TargetId, timeout: Duration) {
        let msg = CdpMessage {
            method: crate::determinism_injector::determinism_injector::VIRTUAL_TIME_METHOD
                .to_string(),
            params:
                crate::determinism_injector::determinism_injector::build_virtual_time_resume_params(
                ),
        };
        // Bound this best-effort cleanup TIGHTLY (a single `setVirtualTimePolicy`
        // round-trip) so it can't add the full navigate budget's worth of latency to
        // an already-failing navigate, and so the cleanup itself can never wedge.
        let resume_to = timeout.min(RESUME_CDP_TIMEOUT);
        if let Err(e) = self.cdp.command(target_id, msg, Some(resume_to)).await {
            tracing::debug!(
                target_id,
                error = %e,
                "navigate: virtual-time resume (advance) failed; renderer may stay paused"
            );
        }
    }

    /// Re-arm the virtual-time budget for a NEW top-level document the page
    /// navigated to itself (client-nav-reattach). Under the determinism clock
    /// pin the new document's load is DEFERRED until a budget is armed (the same
    /// mechanism as the first navigation's STEP 4c). Subscribe to the new
    /// document's `Page.loadEventFired` + `virtualTimeBudgetExpired` BEFORE
    /// arming (so a stale event can't satisfy the wait), arm, then await both —
    /// each bounded by `timeout` (the caller passes the REMAINING action
    /// deadline, so the re-attach loop can never extend wall-clock past the
    /// action budget). Returns `(load_fired, budget_drained)`. Best-effort: a
    /// re-arm command failure logs and returns `(false, false)` so the caller
    /// falls through to a bounded settle → typed `timeout` rather than spinning.
    async fn rearm_for_reattach(&self, target_id: TargetId, timeout: Duration) -> (bool, bool) {
        // The three sequential awaits below (arm command, load wait, budget-drain
        // wait) SHARE a single deadline so one hop can never consume up to 3×
        // `timeout`. `timeout` is the caller's REMAINING action budget, so this
        // keeps the whole re-attach bounded by the action deadline (D10).
        let deadline = tokio::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(tokio::time::Instant::now());

        let (load_rx, _load_reg) = self.subscribe_cdp_event_once("Page.loadEventFired");
        let (vt_expired_rx, _vt_reg) = self.subscribe_cdp_event_once(
            crate::determinism_injector::determinism_injector::VIRTUAL_TIME_BUDGET_EXPIRED_EVENT,
        );
        let vt_budget = CdpMessage {
            method: crate::determinism_injector::determinism_injector::VIRTUAL_TIME_METHOD
                .to_string(),
            params:
                crate::determinism_injector::determinism_injector::build_virtual_time_budget_params(
                ),
        };
        if let Err(e) = self
            .cdp
            .command(target_id, vt_budget, Some(remaining()))
            .await
        {
            tracing::warn!(target_id, error = %e, "reattach: virtual-time budget re-arm failed");
            return (false, false);
        }
        let load_fired = tokio::time::timeout(remaining(), load_rx).await.is_ok();
        // Drain the budget so the re-settle + DOM capture on the new document are
        // virtual-time settled, exactly like STEP 4c does for the first nav.
        let budget_drained = tokio::time::timeout(remaining(), vt_expired_rx)
            .await
            .is_ok();
        (load_fired, budget_drained)
    }

    /// Drive the bounded STEP-4d / D9 client-nav re-attach chain shared by
    /// `page_navigate` and `wait_for`: while the page keeps self-initiating
    /// top-level navigations (`settle.renavigated`), re-arm the virtual-time
    /// budget (under the determinism clock pin) and re-settle on each new
    /// document, hop-capped by [`MAX_REATTACH_HOPS`] and all bounded by the SAME
    /// remaining action deadline (`reattach_start` + `timeout`). Determinism-safe
    /// (NFR-DET-01): `renavigated`/`hops` are shim-internal and never enter the
    /// manifest hash; this only moves code, changing no CDP ordering. Returns the
    /// final settle verdict plus the last re-arm hop's `budget_drained` (see
    /// [`ReattachOutcome`]).
    pub(crate) async fn settle_with_reattach(
        &self,
        target_id: TargetId,
        settle_mode: crate::readiness_monitor::SettleMode,
        timeout: Duration,
        reattach_start: tokio::time::Instant,
        mut settle: crate::readiness_monitor::SettleResult,
        kind: ReattachKind,
    ) -> ReattachOutcome {
        // Only re-arm the virtual-time budget on re-attach when THIS session
        // actually pinned the clock at inject. A `--no-determinism` session runs
        // on the real wall-clock and never pinned it, so re-arming is pointless
        // and harmful: the expiry event never reliably fires on a real-clock
        // page, hanging the re-settle until the deadline.
        let clock_pinned = crate::determinism_injector::determinism_injector::clock_pinned();
        let mut hops: u32 = 0;
        let mut last_drained: Option<bool> = None;
        while settle.renavigated && hops < MAX_REATTACH_HOPS {
            let remaining = timeout.saturating_sub(reattach_start.elapsed());
            if remaining.is_zero() {
                break;
            }
            hops += 1;
            tracing::debug!(
                target_id,
                hops,
                "{}: re-attaching to client-initiated top-level navigation",
                kind.verb()
            );
            let new_load = if clock_pinned {
                let (lf, drained) = self.rearm_for_reattach(target_id, remaining).await;
                last_drained = Some(drained);
                lf
            } else {
                // No determinism clock pin (or `--no-determinism`): the new
                // document loads on its own wall-clock; no budget to re-arm.
                true
            };
            let remaining = timeout.saturating_sub(reattach_start.elapsed());
            if remaining.is_zero() {
                break;
            }
            settle = crate::readiness_monitor::wait_for_settle(
                &self.cdp,
                target_id,
                settle_mode,
                crate::readiness_monitor::settle_driver::config_for_timeout(remaining),
                new_load,
                // the re-attached document was just (re-)loaded above.
                false,
                remaining,
            )
            .await;
        }
        if settle.renavigated {
            // Hit the hop cap or ran out of deadline mid-chain — capture/resolve
            // lands on the current (possibly in-flight) document and the receipt
            // records the bounded `timeout` outcome (D4). Surfaced so a
            // redirect-loop page is distinguishable from a slow one in logs.
            tracing::warn!(
                target_id,
                hops,
                max_hops = MAX_REATTACH_HOPS,
                "{}: re-attach chain exhausted hop cap / deadline; {}",
                kind.verb(),
                kind.exhausted_tail()
            );
        }
        ReattachOutcome {
            settle,
            last_drained,
        }
    }

    pub(super) async fn wait_for_impl(
        &self,
        target_id: TargetId,
        settle_mode: crate::readiness_monitor::SettleMode,
        budget: Option<Duration>,
    ) -> Result<ActionResult, ShimResponse> {
        // Standalone readiness wait on an ALREADY-LOADED page. Unlike navigate
        // we did not subscribe to (and observe) Page.loadEventFired for this
        // call — the page has already loaded by the time a caller invokes
        // wait_for — so `load_fired` is true. The verdict is a pure function of
        // the per-tick observation sequence in virtual ticks (DET-CORE), so it
        // replays identically; the tick ceiling (from `budget`) bounds it to a
        // typed `timeout`/`dom_unstable` instead of hanging.
        // Clamp the requested budget to a sane ceiling so `start + timeout` cannot
        // overflow the monotonic clock on a misconfigured/hostile budget (ship
        // FND-0006). A `None` budget (old-shape payload) falls back to the env base.
        let timeout = budget
            .unwrap_or_else(navigate_budget)
            .min(MAX_SETTLE_TIMEOUT);
        // Zero budget = the daemon is already out of its per-call deadline (the
        // effective-deadline clamp returned 0). Return an immediate typed `timeout`
        // — no CDP with a zero-duration deadline, no resume (ship FND-0014/0020).
        if timeout.is_zero() {
            return Ok(ActionResult::Waited {
                settle_until: settle_mode.as_str().to_string(),
                settle_outcome: crate::readiness_monitor::SettleOutcome::Timeout
                    .as_str()
                    .to_string(),
                settle_ms: 0,
                network_count_at_settle: 0,
            });
        }
        // ONE shared wall-clock deadline for the WHOLE wait — arm, settle, and
        // reattach all draw from it (click-cross-origin-until). Before this, each
        // phase re-took the full `timeout`: the initial VT arm + await, then a
        // FRESH `reattach_start` captured AFTER that arm gave `settle_with_reattach`
        // another full window. On a cross-origin top-level navigation the renderer
        // PROCESS SWAPS, so the budget armed on the stale session never expires and
        // the destination's load never reaches it — every phase dead-waited its
        // full budget, so the shim spent ~2–3× `timeout` and blew past the RPC
        // per-call deadline (the v0.15.0 `request_timeout` on a click whose
        // navigation actually succeeded). Sharing one deadline bounds these phases
        // to ~1× `timeout` and guarantees a TYPED verdict, never a transport error.
        // The post-settle RESUME cleanup below is deliberately NOT drawn from this
        // deadline — it gets its own bounded slice so it can always run (ship
        // FND-0001); the daemon's settle budget reserves headroom for it. The
        // `remaining` closure is the single source of the per-phase bound below.
        let start = tokio::time::Instant::now();
        let deadline = start + timeout;
        let remaining = || deadline.saturating_duration_since(tokio::time::Instant::now());
        // Gate the virtual-time arm+await on whether this session actually pinned
        // the clock at inject. A `--no-determinism` session never pinned it, so
        // arming a budget here would await a `virtualTimeBudgetExpired` that never
        // reliably fires on the real wall-clock (e.g. while a cross-origin
        // silent-auth iframe keeps network fetches pending) — the wedge behind the
        // 2nd authed action on a heavy SPA. (No replay impact: non-replayable.)
        let clock_pinned = crate::determinism_injector::determinism_injector::clock_pinned();

        // settle-drives-pending-timers (auth0-ulp-submit): under the determinism
        // clock pin the virtual clock is FROZEN here — the prior navigate/settle
        // drained its budget and a cleanly-drained budget is deliberately left
        // paused (see page_navigate STEP 4c / the exit-resume note: only an
        // UNDRAINED budget resumes to `advance`). A preceding interaction verb
        // (web.click / web.press_key — host-side raw `Input.*` passthroughs that
        // arm no budget of their own) can run a page handler that schedules async
        // work behind a timer: e.g. Auth0 New Universal Login's react-hook-form
        // onSubmit does async validate → `navigator.credentials` WebAuthn probe →
        // `fetch(POST /u/login/identifier)`. With the clock frozen that chain
        // stalls at its first macrotask/`setTimeout` await — BEFORE it ever issues
        // the request — so the page never begins the navigation this wait_for is
        // meant to observe, and the old "no-vt-arm common path" settled the still-
        // `complete` document immediately (the bug).
        //
        // Arm a bounded virtual-time budget HERE — the same helper + ordering
        // navigate STEP 4c uses (subscribe to the expiry BEFORE arming so a stale
        // event can't satisfy the wait) — so pending timers fire and any resulting
        // top-level navigation begins. The settle + reattach below then observe and
        // resolve it on the new document. Determinism-safe (NFR-DET-01): the VT
        // control commands are shim-internal and excluded from the manifest hash
        // (navigate arms budgets the same way); the settle verdict stays a pure
        // function of the deterministic per-tick observation sequence, so it is
        // replay-equal. A page with no pending timers drains the budget immediately
        // (one extra CDP round-trip), unchanged verdict.
        let mut initial_budget_drained = true;
        if clock_pinned {
            let (vt_expired_rx, _vt_reg) = self.subscribe_cdp_event_once(
                crate::determinism_injector::determinism_injector::VIRTUAL_TIME_BUDGET_EXPIRED_EVENT,
            );
            let vt_budget = CdpMessage {
                method: crate::determinism_injector::determinism_injector::VIRTUAL_TIME_METHOD
                    .to_string(),
                params:
                    crate::determinism_injector::determinism_injector::build_virtual_time_budget_params(
                    ),
            };
            // Bound the arm + await by the SHARED remaining budget, not a fresh
            // full `timeout`. A cross-origin swap that eats the whole remaining
            // here leaves the phases below with ~zero budget, so they return a
            // typed verdict immediately instead of each re-taking a full window.
            if let Err(e) = self
                .cdp
                .command(target_id, vt_budget, Some(remaining()))
                .await
            {
                tracing::warn!(target_id, error = %e, "wait_for: virtual-time budget arm failed");
                initial_budget_drained = false;
            } else {
                initial_budget_drained = tokio::time::timeout(remaining(), vt_expired_rx)
                    .await
                    .is_ok();
            }
        }

        // Reattach measures its remaining window from the TRUE `start` (shared
        // deadline), NOT a fresh instant captured after the arm above — that reset
        // was the multi-phase overrun. `settle_with_reattach` computes
        // `timeout.saturating_sub(reattach_start.elapsed())`, so passing `start`
        // makes its window the same shared remaining. Evaluate `remaining()` ONCE
        // so the config ceiling and the timeout arg agree (ship FND-0003).
        let rem = remaining();
        let mut settle = crate::readiness_monitor::wait_for_settle(
            &self.cdp,
            target_id,
            settle_mode,
            crate::readiness_monitor::settle_driver::config_for_timeout(rem),
            true,
            // the caller asserts the page already loaded, so a `loading`
            // observation here means the page renavigated (e.g. a form-POST
            // submitted just before this wait_for) — detect + re-attach.
            true,
            rem,
        )
        .await;

        // client-nav-reattach (D9): the initial budget arm above advanced pending
        // timers; if that began a top-level navigation the settle reads `loading`
        // and the vt-arm reattach path re-settles on the new document, giving
        // wait_for the same bounded re-attach as navigate instead of hanging.
        let outcome = self
            .settle_with_reattach(
                target_id,
                settle_mode,
                timeout,
                start,
                settle,
                ReattachKind::WaitFor,
            )
            .await;
        settle = outcome.settle;
        // A budget left undrained — either the initial arm above (arm failure /
        // drain timeout) or the final reattach hop — would wedge the next command
        // on a paused clock mid-flight, so resume to `advance` on exit. A cleanly
        // drained budget (the common case) is deliberately left paused/frozen,
        // matching navigate's clean-path exit so a subsequent web.evaluate clock
        // read stays replay-equal.
        let needs_resume = !initial_budget_drained || matches!(outcome.last_drained, Some(false));
        if clock_pinned && needs_resume {
            // Give resume its OWN bounded slice (RESUME_CDP_TIMEOUT), NOT the
            // leftover `remaining()`. A cross-origin swap consumes the whole settle
            // budget, so `remaining()` is ~zero here — drawing resume from it would
            // STARVE the cleanup, leaving the virtual clock paused and wedging the
            // NEXT command (ship FND-0001). The daemon's settle budget reserves
            // headroom for this slice (SETTLE_HEADROOM_MS ≥ RESUME_CDP_TIMEOUT), so
            // `settle + resume` still lands inside the RPC deadline. On a healthy
            // renderer the resume is a single sub-ms round-trip; the cap only bites
            // if the ack never comes.
            self.resume_virtual_time(target_id, RESUME_CDP_TIMEOUT)
                .await;
        }

        Ok(ActionResult::Waited {
            settle_until: settle.mode.as_str().to_string(),
            settle_outcome: settle.outcome.as_str().to_string(),
            settle_ms: settle.ticks as u64 * 5,
            network_count_at_settle: settle.network_count as u64,
        })
    }
}
