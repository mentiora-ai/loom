// `ActionExecutor::page_navigate` for Chromium — the navigate-and-settle-capture
// sequence; the trait method delegates here. Moved verbatim.

use crate::action_executor::action_executor::action_error_to_response;
use crate::action_executor::action_executor::cbor_to_bytes;
use crate::action_executor::action_executor::navigate_budget;
use crate::action_executor::action_executor::sha256_hex_of_bytes;
use crate::action_executor::action_executor::should_resume_virtual_clock;
use crate::action_executor::action_executor::ActionError;
use crate::action_executor::action_executor::ActionResult;
use crate::action_executor::action_executor::ChromiumActionExecutor;
use crate::action_executor::page_extract::extract_console_line;
use crate::action_executor::page_extract::extract_frame_loader;
use crate::action_executor::page_extract::extract_nav_error_text;
use crate::action_executor::page_extract::extract_title_and_url_from_evaluate;
use crate::action_executor::page_extract::find_main_document_index;
use crate::action_executor::settle::ReattachKind;
use crate::cdp_connection::cdp_connection::{CdpConnection, EventFilter, EventHandler};
use crate::dispatcher::dispatcher::make_error_response;
use crate::ipc_endpoint::ipc_endpoint::{CdpMessage, ShimErrorCode, ShimResponse, TargetId};
use crate::network_interceptor::network_interceptor::{
    classify_chromium_nav_error, LoomNetworkEvent, NetworkInterceptor,
};
use crate::target_manager::target_manager::TargetManager;
use ciborium::value::Value as CborValue;
use loom_shared::navigate_outcome::ShimConsoleLine;
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;

impl ChromiumActionExecutor {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn page_navigate_impl(
        &self,
        target_id: TargetId,
        url: String,
        budget: Option<Duration>,
        blocklist_enabled: bool,
        settle_mode: crate::readiness_monitor::SettleMode,
        determinism_enabled: bool,
        audio_enabled: bool,
    ) -> Result<ActionResult, ShimResponse> {
        // R3 PRECONDITION. Refuse to navigate before the
        // determinism script has been installed for this target. Today
        // the flag flips true ONLY on Ok(()) from `inject` (per
        // target_manager); a false flag here would mean a regression
        // re-introduced the silent-success path. Defense-in-depth.
        if !self.targets.determinism_ready(target_id) {
            tracing::error!(target_id, "R3OrderingViolation: navigate before inject");
            return Err(make_error_response(
                0,
                None,
                ShimErrorCode::ShimInternalError,
                "R3OrderingViolation",
            ));
        }

        // voice-call-io (task 03): for an `--audio` session, grant `audioCapture`
        // scoped to THIS navigation's origin (D16 — never all-origins) BEFORE
        // issuing `Page.navigate`, so the permission is in place when the app's
        // first `getUserMedia({audio})` fires. Browser-scope method (no
        // sessionId). BEST-EFFORT: a failure only WARNs (mirrors the STEP 5
        // setDownloadBehavior precedent) — the `--use-fake-device` beep is the
        // benign fallback. Non-http(s) targets (about:blank/data:) have no origin
        // to grant, so the grant is skipped. The successful grant is recorded so
        // the clean close path issues `Browser.resetPermissions`.
        if audio_enabled {
            if let Some(origin) = crate::cdp_connection::cdp_connection::grant_origin_for_url(&url)
            {
                let grant_msg = CdpMessage {
                    method: crate::cdp_connection::cdp_connection::GRANT_PERMISSIONS_METHOD
                        .to_string(),
                    params: crate::cdp_connection::cdp_connection::build_grant_audio_capture_params(
                        &origin,
                    ),
                };
                match self.cdp.command(target_id, grant_msg, budget).await {
                    Ok(_) => {
                        crate::cdp_connection::cdp_connection::mark_audio_capture_granted();
                        tracing::info!(
                            target_id,
                            origin = %origin,
                            "audio: Browser.grantPermissions(audioCapture) granted for origin"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target_id,
                            origin = %origin,
                            error = %e,
                            "audio: Browser.grantPermissions(audioCapture) failed; relying on \
                             --use-fake-device fallback (getUserMedia may prompt or beep)"
                        );
                    }
                }
            }
        }

        // Tighter default for navigate so unreachable hosts surface a
        // typed-error receipt within the network-error budget window. Callers
        // passing an explicit `budget` keep their value; otherwise the
        // env-configurable `navigate_budget()` applies (the dispatcher passes
        // `None`, so it is the binding per-CDP-command timeout in production).
        let timeout = budget.unwrap_or_else(navigate_budget);

        // STEP 1: subscribe to Page.loadEventFired BEFORE issuing navigate
        // (per practitioner bug magnet #1: cached / data: URLs fire fast,
        // and post-navigate subscription will miss the event). The RAII token
        // keeps the handler registered for this navigate only.
        let (load_rx, _load_reg) = self.subscribe_cdp_event_once("Page.loadEventFired");
        // (The virtual-time budget-expiry subscription happens in STEP 4c,
        // immediately before the budget is armed — NOT here. Subscribing at
        // navigate start left a multi-second window in which a STALE
        // `virtualTimeBudgetExpired` from a previous navigate's never-drained
        // budget could satisfy this navigate's wait before its budget was even
        // armed, letting DOM capture race the fresh budget.)

        // Reset the full-capture network-entries accumulator at navigate START
        // so this navigate's `network_entries` reflect only this navigate;
        // entries then accumulate across in-session clicks/evaluate until the
        // next navigate. CDP events arrive under target_id==0 (see the drain
        // note below); clear both keys to cover the fake-chromium per-target path.
        self.network.clear_entries(0);
        self.network.clear_entries(target_id);

        // Also drop any HASHED Document events left over from before this
        // navigate: a failed/aborted prior navigate early-returns past the
        // STEP 7 drain, and an in-session click can trigger a real link
        // navigation whose Document events land between navigates. Without
        // this, stale events poison the next receipt's `network_events` /
        // `status_code` (and the host's 4xx/transport failure verdict).
        // Clearing at START opens the hashed event window at Page.navigate
        // issue time, making capture per-navigation deterministic
        // (NFR-DET-01); the success path drained everything at STEP 7
        // already, so serial successful navigates record byte-identical
        // receipts either way.
        self.network.clear_events(0);
        self.network.clear_events(target_id);

        // STEP 1b: subscribe to Runtime.consoleAPICalled to accumulate
        // console output during the navigate.. The
        // collector is dropped at the end of this fn so each navigate
        // gets a fresh log; cross-action persistence is not part of the
        // brief.
        let console_collector: Arc<parking_lot::Mutex<Vec<ShimConsoleLine>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let console_for_handler = console_collector.clone();
        let console_handler: EventHandler = Arc::new(move |_target_id, msg: CdpMessage| {
            if msg.method == "Runtime.consoleAPICalled" {
                if let Some(line) = extract_console_line(&msg.params) {
                    console_for_handler.lock().push(line);
                }
            }
        });
        let _console_reg = self.cdp.register_event_handler(
            EventFilter::new("Runtime.consoleAPICalled"),
            console_handler,
        );

        // STEP 2a: subscribe to Network.responseReceived to capture network events.
        // Stub: NetworkInterceptor handles its own event subscription at boot,
        // so we just snapshot the events accumulated so far at navigate end.

        // STEP 2b: subscribe to Fetch.requestPaused for sub-resource
        // blocklist enforcement. Skipped
        // when the operator passed --no-blocklist (decoded into
        // `blocklist_enabled = false` on the wire). The interceptor's
        // own constructor short-circuits when its blocklist is empty,
        // so the cost of subscribing per-navigate is just one
        // `Fetch.enable` CDP roundtrip when enabled.
        if blocklist_enabled {
            if let Err(e) = self.network.subscribe(target_id).await {
                tracing::warn!(target_id, error = %e, "blocklist Fetch.enable failed; sub-resources will not be gated for this navigate");
            }
        }

        // STEP 3: send Page.navigate.
        let mut nav_params = vec![
            (CborValue::Text("url".into()), CborValue::Text(url.clone())),
            (
                CborValue::Text("transitionType".into()),
                CborValue::Text("typed".into()),
            ),
        ];
        // Drop the borrow checker shenanigans: just construct directly.
        let nav_msg = CdpMessage {
            method: "Page.navigate".into(),
            params: CborValue::Map(std::mem::take(&mut nav_params)),
        };
        let nav_response = self
            .cdp
            .command(target_id, nav_msg, Some(timeout))
            .await
            .map_err(|e| action_error_to_response(ActionError::Cdp(e), 0, None))?;
        let (frame_id, loader_id) = extract_frame_loader(&nav_response);

        // STEP 4 + 4c: await Page.loadEventFired and (under virtual time) arm
        // + drain this navigation's bounded virtual-time budget
        // (faithful-entrance-animations + cross-run determinism). The budget is
        // a hard ceiling (security boundary) so a never-terminating animation
        // pauses instead of spinning the renderer unbounded.
        //
        // ORDER IS LOAD-BEARING and depends on whether the inject-time clock
        // pin is in effect:
        //
        // - determinism ON: inject sent `setVirtualTimePolicy {policy:"pause"}`,
        //   and headless Chromium DEFERS the load-completion tasks while
        //   virtual time is paused — `Network.responseReceived` arrives but
        //   `Page.loadEventFired` is held until the clock advances (verified
        //   live: the load event fires the instant the budget is armed).
        //   Waiting for load BEFORE arming therefore always burned the full
        //   wall-clock timeout, and the settle machine — whose networkidle/
        //   settled latches require `load_fired` — then reported `timeout` on
        //   perfectly healthy static pages (settle-timeout-on-static). So under
        //   determinism: arm the budget FIRST (it is what lets the load
        //   complete), then await load, then await the budget expiry so DOM
        //   capture stays virtual-time settled (commit 114be83).
        // - determinism OFF: the clock was never pinned (inject skipped), so
        //   keep the original order — await load, then arm post-load (arming
        //   before the load is in flight can make the expiry event unreliable
        //   under pauseIfNetworkFetchesPending, chrome-headless-render-pdf#29).
        let vt_active = crate::determinism_injector::determinism_injector::virtual_time_enabled();
        let clock_pinned = vt_active && determinism_enabled;

        let mut load_rx = Some(load_rx);
        let mut load_fired = if clock_pinned {
            // Load cannot complete while the inject-time pin holds; resolved
            // after the budget is armed below.
            false
        } else {
            // The fake-chromium harness emits the event right after the
            // Page.navigate response so this typically completes immediately.
            // Real Chromium can take longer.
            let rx = load_rx.take().expect("load_rx consumed once");
            tokio::time::timeout(timeout, rx).await.is_ok()
        };

        // animation-capture (Mode A): tracks whether THIS navigate's virtual-time
        // budget was confirmed drained. A cleanly drained budget leaves the
        // renderer at a screenshottable horizon with a frozen (deterministic)
        // clock — no resume needed, so determinism is preserved on the clean path.
        // A NOT-drained budget (rearm fail / drain timeout / capture error) can
        // leave the renderer paused mid-flight, wedging the next command for the
        // full 30s CDP timeout — so we resume (`advance`) on exit ONLY then.
        let mut budget_drained = false;

        // Only drive virtual time when THIS session actually pinned the clock at
        // inject (`clock_pinned`). A `--no-determinism` session has `vt_active`
        // true (the global capture flag) but runs on the REAL wall-clock — arming
        // a virtual-time budget here makes `virtualTimeBudgetExpired` unreliable
        // (it never fires while a cross-origin iframe keeps network fetches
        // pending under `pauseIfNetworkFetchesPending`), so navigate burned the
        // full timeout and, on a heavy auth'd SPA, wedged the next navigate. Gate
        // on `clock_pinned`, not `vt_active`, so no_determinism takes the clean
        // real-clock load+settle path. (No replay impact: no_determinism sessions
        // are non-replayable.)
        if clock_pinned {
            // Subscribe to the budget-expiry event immediately BEFORE issuing
            // the arm command — after the prior navigate's awaits, not at
            // navigate start. The WS command/response round-trip orders this
            // subscription ahead of THIS navigate's expiry event, while
            // shrinking the window in which a stale expiry from a previous
            // navigate's never-drained budget (the warn path below) could
            // satisfy the wait prematurely from seconds to sub-millisecond.
            // (A stale `Page.loadEventFired` has the same residual hazard;
            // with the pin-aware ordering above the load wait can no longer
            // time out on healthy pages, which is what left budgets undrained.)
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
            if let Err(e) = self.cdp.command(target_id, vt_budget, Some(timeout)).await {
                // Best-effort: the page stays on the inject clock. Under the
                // pin that also means the load CANNOT complete — skip the load
                // wait (it would burn the full timeout) and fall through to
                // the bounded settle, which returns a typed `timeout`.
                tracing::warn!(target_id, error = %e, "navigate: virtual-time budget re-arm failed");
            } else {
                // `load_rx` is still Some exactly when the clock was pinned
                // (the not-pinned branch consumed it pre-arm).
                if let Some(rx) = load_rx.take() {
                    load_fired = tokio::time::timeout(timeout, rx).await.is_ok();
                }
                // animation-capture (Mode C, C3): block DOM capture until the
                // virtual-time budget drains REGARDLESS of `determinism_enabled`.
                // Previously this awaited only under determinism, so a
                // `--no-determinism` (or budget-rearm) capture raced the reveal and
                // grabbed a pre-reveal `opacity:0` frame. All ≤budget virtual timers
                // (e.g. a setTimeout/whileInView reveal) fire first, making the
                // captured DOM stable. Bounded-determinism: a pathological page can
                // exhaust the wall-clock timeout before the budget elapses; on
                // timeout we warn + fall through to the existing settle (non-fatal)
                // so capture never hangs (D-INTAKE), and `budget_drained` stays
                // false so the exit guard resumes the renderer.
                budget_drained = tokio::time::timeout(timeout, vt_expired_rx).await.is_ok();
                if budget_drained {
                    tracing::debug!(
                        target_id,
                        "navigate: virtualTimeBudgetExpired arrived; DOM capture is virtual-time settled"
                    );
                } else {
                    tracing::warn!(
                        target_id,
                        "navigate: virtualTimeBudgetExpired did not arrive within timeout; \
                         settle may be non-deterministic — falling back to wait_for_settle"
                    );
                }
            }
        }

        // STEP 4b (settle-capture): gate the capture on the requested readiness
        // state. The verdict is a pure function of the per-tick observation
        // sequence in virtual ticks (DET-CORE), so it is replay-equal. `load` mode
        // returns immediately once load fired; networkidle/settled poll until quiet
        // or the tick ceiling (a bounded, typed fallback). animation-
        // capture Mode C: intersection-gated `whileInView` reveals fire AT MOUNT
        // (the deterministic IntersectionObserver override installed at inject —
        // see `determinism_injector::REVEAL_IO_OVERRIDE_JS`), so by the time the
        // budget above has drained they have animated to completion and the settled
        // capture below is post-reveal, not a pre-reveal blank.
        let reattach_start = tokio::time::Instant::now();
        let mut settle = crate::readiness_monitor::wait_for_settle(
            &self.cdp,
            target_id,
            settle_mode,
            crate::readiness_monitor::settle_driver::config_for_timeout(timeout),
            load_fired,
            // navigate's first settle ticks may transiently read `loading`
            // before the freshly-loaded document reflects `complete`.
            false,
            timeout,
        )
        .await;

        // STEP 4d (client-nav-reattach): if the loaded page began a top-level
        // navigation it initiated itself (window.location / <meta refresh> /
        // form-POST), the new document is wedged `loading` under the paused
        // virtual clock. Re-arm its budget and re-settle on it, following a
        // bounded redirect chain under the SAME action deadline (D4). DOM capture
        // below then lands on the FINAL settled document, not the blank shell.
        let outcome = self
            .settle_with_reattach(
                target_id,
                settle_mode,
                timeout,
                reattach_start,
                settle,
                ReattachKind::Navigate,
            )
            .await;
        settle = outcome.settle;
        // Carry the final re-arm hop's drained state back to the renderer-resume
        // guards below (only changes when a vt-arm hop actually ran).
        if let Some(drained) = outcome.last_drained {
            budget_drained = drained;
        }

        // STEP 5: DOM.getDocument → raw CBOR bytes + SHA-256.
        let dom_msg = CdpMessage {
            method: "DOM.getDocument".into(),
            params: CborValue::Map(vec![
                (
                    CborValue::Text("depth".into()),
                    CborValue::Integer((-1i64).into()),
                ),
                (CborValue::Text("pierce".into()), CborValue::Bool(true)),
            ]),
        };
        let dom_result = match self.cdp.command(target_id, dom_msg, Some(timeout)).await {
            Ok(r) => r,
            Err(e) => {
                // animation-capture (Mode A): un-pause before bailing so the next
                // command isn't wedged on a paused clock (unless we are on the
                // determinism-pinned clean-drain path — see the success-path guard).
                if should_resume_virtual_clock(clock_pinned, clock_pinned, budget_drained) {
                    self.resume_virtual_time(target_id, timeout).await;
                }
                return Err(action_error_to_response(ActionError::Cdp(e), 0, None));
            }
        };
        // Strip the ephemeral per-navigation `frameId` (and any future ephemeral
        // CDP id) from the DOM CBOR before it is hashed or stored, so two
        // independent same-seed captures of byte-identical content produce the
        // same `dom_snapshot_hash`. The host re-hashes these stored bytes, so the
        // bytes themselves (not just the side hash) must be normalized.
        let dom_bytes = loom_shared::dom_normalize::normalize_dom_cbor(&cbor_to_bytes(&dom_result))
            .into_bytes();
        let dom_after_sha256 = sha256_hex_of_bytes(&dom_bytes);

        // STEP 6: Page.captureScreenshot → raw CBOR bytes + SHA-256.
        let shot_msg = CdpMessage {
            method: "Page.captureScreenshot".into(),
            params: CborValue::Map(vec![(
                CborValue::Text("format".into()),
                CborValue::Text("png".into()),
            )]),
        };
        let shot_result = match self.cdp.command(target_id, shot_msg, Some(timeout)).await {
            Ok(r) => r,
            Err(e) => {
                // animation-capture (Mode A): un-pause before bailing (see STEP 5
                // and the success-path guard for the determinism-pinned exception).
                if should_resume_virtual_clock(clock_pinned, clock_pinned, budget_drained) {
                    self.resume_virtual_time(target_id, timeout).await;
                }
                return Err(action_error_to_response(ActionError::Cdp(e), 0, None));
            }
        };
        // Decode the CDP screenshot envelope (CBOR `{data: <base64-PNG>}`)
        // into raw PNG bytes so the content store holds a renderable image
        // rather than a double-encoded envelope. On the rare decode failure
        // fall back to the raw CBOR bytes (pre-fix behaviour) so navigate
        // never breaks on an unexpected response shape.
        let raw_cbor = cbor_to_bytes(&shot_result);
        let screenshot_bytes = match loom_shared::screenshot_decode::decode_cdp_screenshot(
            &raw_cbor,
        ) {
            Ok(png) => png,
            Err(e) => {
                tracing::warn!(error = %e, "screenshot decode failed; storing raw CDP envelope");
                raw_cbor
            }
        };
        let screenshot_sha256 = sha256_hex_of_bytes(&screenshot_bytes);

        // STEP 6a: extract document.title and final_url via a single
        // Runtime.evaluate roundtrip. This replaces the earlier stubs
        // that left both fields empty. Both
        // are best-effort: a CDP error here doesn't fail the navigate
        // — we just leave the field empty (same as the stub it
        // replaces). Single roundtrip keeps the cost ~constant vs
        // separate calls.
        let (page_title, real_final_url) = {
            let eval_msg = CdpMessage {
                method: "Runtime.evaluate".into(),
                params: CborValue::Map(vec![
                    (
                        CborValue::Text("expression".into()),
                        CborValue::Text(
                            "JSON.stringify([document.title || '', \
                              location.href || ''])"
                                .into(),
                        ),
                    ),
                    (
                        CborValue::Text("returnByValue".into()),
                        CborValue::Bool(true),
                    ),
                    (
                        CborValue::Text("awaitPromise".into()),
                        CborValue::Bool(false),
                    ),
                ]),
            };
            match self.cdp.command(target_id, eval_msg, Some(timeout)).await {
                Ok(r) => extract_title_and_url_from_evaluate(&r)
                    .unwrap_or_else(|| (String::new(), url.clone())),
                Err(_) => (String::new(), url.clone()),
            }
        };

        // STEP 7: drain network events + blocked sub-resource events
        // accumulated by NetworkInterceptor. Blocked events become
        // manifest `AuditEntry { kind: BlockedUrl }` on the host side.
        //
        // CDP envelope events arrive at handlers with `target_id == 0`
        // (cdp_connection's read_loop hardcodes that — there's no
        // multi-target routing today; one chromium subprocess per
        // session, one CDP session). NetworkInterceptor::append stores
        // events under `target_id == 0`, so we drain from there too.
        // Falling back to the requested `target_id` keeps the
        // fake-chromium harness's per-target drain working — that
        // path uses real per-target IDs.
        let mut drained = self.network.drain_events_attributed(0);
        if drained.is_empty() {
            drained = self.network.drain_events_attributed(target_id);
        }
        let mut blocked_events = self.network.drain_blocked(0);
        if blocked_events.is_empty() {
            blocked_events = self.network.drain_blocked(target_id);
        }

        // Identify THIS navigation's main-document event by matching each
        // event's frame/loader attribution against the Page.navigate
        // response's frameId/loaderId. Iframe document events (different
        // frame/loader) stay in `network_events` for observability but
        // must not drive `status_code` or the host's failure verdict.
        let mut main_document_event_index =
            find_main_document_index(&drained, &frame_id, &loader_id);
        let mut network_events: Vec<LoomNetworkEvent> =
            drained.into_iter().map(|(event, _)| event).collect();

        // Read (NON-draining) the full-capture network-entries snapshot — the
        // accumulator persists across in-session actions for the `network_log`
        // tool. Mirror the target_id==0 / target_id duality used above.
        let (network_entries, network_entries_truncated) = {
            let from_zero = self.network.read_entries(0);
            if from_zero.0.is_empty() {
                self.network.read_entries(target_id)
            } else {
                from_zero
            }
        };

        // Surface DNS / network failures via a synthetic LoomNetworkEvent
        // carrying `error_reason`. CDP `Page.navigate` returns a non-empty
        // `errorText` field when navigation could not even reach a server
        // (DNS, conn-refused, TLS, etc.). The host-side `navigate_execute`
        // detects a main-document event with `error_reason.is_some()` and
        // converts it into an HostError::ShimFailure carrying a structured
        // JSON detail (kind="dns_failure").
        if let Some(err_text) = extract_nav_error_text(&nav_response) {
            // Classify at the boundary (D-01): the shim owns chromium-
            // specific error mapping, the host reads the typed kind.
            let kind = classify_chromium_nav_error(&err_text).to_string();
            network_events.push(LoomNetworkEvent {
                // A page navigation is always a GET; populate it so failed-run HAR
                // evidence carries the method (deterministic → safe in the hash).
                // No HTTP response → response_bytes stays 0.
                method: "GET".to_string(),
                url: url.clone(),
                request_hash: String::new(),
                response_hash: String::new(),
                status: 0,
                content_type: String::new(),
                duration_ms: 0,
                response_bytes: 0,
                error_reason: Some(err_text),
                error_kind: Some(kind),
            });
            // The navigation itself failed, so the synthetic event IS the
            // main-document verdict — UNLESS a captured main-document
            // response already carries the HTTP status (chromium emits
            // ERR_HTTP_RESPONSE_CODE_FAILURE alongside a 4xx/5xx
            // responseReceived for empty-body responses; the actual
            // status_code is the more actionable verdict — see the
            // HTTP-first ordering note in `navigate_execute`).
            let has_http_verdict = main_document_event_index
                .and_then(|i| network_events.get(i as usize))
                .is_some_and(|e| e.status >= 400);
            if !has_http_verdict {
                main_document_event_index = Some((network_events.len() - 1) as u32);
            }
        }

        // Derive status_code from the MAIN document's event; fall back to 0
        // when no event is attributable to the main document. The synthetic
        // error event above has status=0, so a transport failure reports
        // status_code=0 exactly as before.
        let status_code = main_document_event_index
            .and_then(|i| network_events.get(i as usize))
            .map(|e| e.status)
            .unwrap_or(0);

        // animation-capture (Mode A): on the SUCCESS path resume the renderer
        // UNLESS we are on the determinism-pinned clean-drain path. A cleanly
        // drained budget UNDER the determinism clock pin leaves a screenshottable
        // horizon with a frozen, deterministic clock, so we DON'T resume it
        // (preserving replay-equality for a subsequent web.evaluate clock read).
        // With determinism OFF there is no replay contract, so a frozen clock is
        // pure harm: it defers the NEXT navigate's Page.loadEventFired and wedges
        // the determinism-OFF navigate path into burning its full budget every
        // call (navigate-degradation). `should_resume_virtual_clock` encodes the
        // policy once for all three exit guards.
        if should_resume_virtual_clock(clock_pinned, clock_pinned, budget_drained) {
            self.resume_virtual_time(target_id, timeout).await;
        }

        Ok(ActionResult::Navigated {
            target_id,
            frame_id,
            loader_id,
            network_events,
            dom_after_sha256,
            screenshot_sha256,
            url: url.clone(),
            // real final_url from `location.href` (post-
            // redirect; falls back to requested URL on CDP error).
            final_url: real_final_url,
            // real page_title from `document.title`
            // (empty string is a legitimate value when the page has no
            // <title> — distinguishable from the prior unconditional stub
            // because final_url is no longer == url for redirecting pages).
            page_title,
            status_code,
            main_document_event_index,
            dom_bytes,
            screenshot_bytes,
            // console_lines populated from the
            // Runtime.consoleAPICalled events that arrived during this
            // navigate. The collector was subscribed BEFORE Page.navigate
            // so messages fired during page load (cached / data: URLs
            // included) are captured.
            console_lines: {
                let mut guard = console_collector.lock();
                std::mem::take(&mut *guard)
            },
            settle_until: settle.mode.as_str().to_string(),
            settle_outcome: settle.outcome.as_str().to_string(),
            // Map the virtual tick count to a ms diagnostic via the pacing
            // cadence. Excluded from outcome_hash host-side.
            settle_ms: settle.ticks as u64 * 5,
            network_count_at_settle: settle.network_count as u64,
            blocked_events,
            network_entries,
            network_entries_truncated,
        })
    }
}
