// ShimManager — locator resolution: `css=` / `frame=` descent over the DOM
// domain, `text=` / `role=` through the marker resolver (`locator_js`), and the
// deadline-bounded `web.wait` poll built on it.

use super::helpers::{cbor_get, cbor_u64};
use super::locator_js::{marker_resolver_js, MARKER_ATTR, MARKER_SELECTOR};
use super::shim_manager::ShimManager;
use super::types::{FailureClass, ShimId, WaitResolveOutcome};
use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::locator::{parse_locator, Segment};
use loom_shared::shim_protocol::CdpMessage;
use std::time::Duration;

/// `web.wait` deadline when the caller omits `timeout_ms` (the action_registry
/// docs already promise "typically 30 s").
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Hard ceiling on a `web.wait` deadline — clamps a pathological / runaway
/// `timeout_ms` so a single wait can't pin a session indefinitely.
const MAX_WAIT_TIMEOUT_MS: u64 = 600_000;

/// Re-probe cadence for `web.wait` locator resolution. Sequential (each probe is
/// awaited before the next sleep), so this is a floor on the gap between probes,
/// not a concurrent fan-out — it keeps the renderer/transport load modest.
const WAIT_POLL_INTERVAL_MS: u64 = 100;

impl ShimManager {
    /// Always-trusted `web.click` — resolve the element's hit point (box-model
    /// center, scrolling into view first) and dispatch a trusted
    /// `DOM.querySelector(root, css)` → `Some(nodeId)` (a 0 nodeId ⇒ `None`).
    async fn dom_query_selector(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        root: u64,
        css: &str,
        budget_ms: u64,
    ) -> Result<Option<u64>, LoomError> {
        use ciborium::value::{Integer, Value};
        let Ok(qs) = self
            .cdp_send_one(
                id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.querySelector".into(),
                    params: Value::Map(vec![
                        (
                            Value::Text("nodeId".into()),
                            Value::Integer(Integer::from(root)),
                        ),
                        (Value::Text("selector".into()), Value::Text(css.to_string())),
                    ]),
                },
                budget_ms,
            )
            .await?
        else {
            return Ok(None); // app error ⇒ no match (see resolve_locator_node)
        };
        let node = cbor_get(&qs, "nodeId").and_then(cbor_u64).unwrap_or(0);
        Ok(if node == 0 { None } else { Some(node) })
    }

    /// Resolve a (possibly `frame=`-prefixed) locator to a DOM `nodeId` in the
    /// page session, descending through same-process iframes (incl. same-site
    /// cross-origin) via `DOM.describeNode{pierce:true}` → `contentDocument`.
    /// CDP is not bound by the same-origin policy, so a cross-origin (but
    /// in-process) frame's content is reachable this way — the
    /// `iframe.contentDocument === null` blocker is a page-JS limitation, not a
    /// CDP one.
    ///
    /// Returns `Ok(Some(nodeId))` on a match; `Ok(None)` when nothing matched, the
    /// frame is out-of-process (no in-process `contentDocument`), or the leaf is a
    /// `text=`/`role=` form (resolved by the evaluate-tier resolver, not this DOM
    /// path). `Err` only on transport failure.
    ///
    /// A CDP *application* error while resolving is `Ok(None)` too: Chromium
    /// rejecting a selector it cannot parse (`DOM Error while querying`, e.g.
    /// Playwright's `:text()`), or a node / execution context that went away
    /// mid-resolution. The page offers no match. It must never be an `Err`,
    /// because every caller records an `Err` as a TRANSPORT failure, which evicts
    /// the shim and kills the session's live browser: one invalid selector used to
    /// end a whole studio run (hollie staging, demo-run-32ae7f20).
    pub(super) async fn resolve_locator_node(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        selector: &str,
        budget_ms: u64,
    ) -> Result<Option<u64>, LoomError> {
        use ciborium::value::{Integer, Value};

        let segments = match parse_locator(selector) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };

        // A single `text=`/`role=` locator resolves in the top frame's default
        // execution context via a marker-attribute resolver (the common
        // testid-less case, e.g. a shadcn button). Composed (`… >> text=`) and
        // in-(cross-origin-)frame text/role are a follow-up — they need the
        // frame's executionContextId.
        if let [seg @ (Segment::Text(_) | Segment::Role(_))] = segments.as_slice() {
            return self
                .resolve_marked_node(id, session_id, target_id, seg, budget_ms)
                .await;
        }

        let Ok(doc) = self
            .cdp_send_one(
                id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.getDocument".into(),
                    params: Value::Map(vec![(
                        Value::Text("depth".into()),
                        Value::Integer(Integer::from(0)),
                    )]),
                },
                budget_ms,
            )
            .await?
        else {
            return Ok(None); // app error ⇒ no match (see resolve_locator_node)
        };
        let mut root = cbor_get(&doc, "root")
            .and_then(|r| cbor_get(r, "nodeId"))
            .and_then(cbor_u64)
            .ok_or_else(|| {
                LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: getDocument: no root.nodeId", id.0),
                )
            })?;

        let last = segments.len() - 1;
        for (i, seg) in segments.iter().enumerate() {
            let is_last = i == last;
            match seg {
                Segment::Frame(css) => {
                    let iframe = match self
                        .dom_query_selector(id, session_id, target_id, root, css, budget_ms)
                        .await?
                    {
                        Some(n) => n,
                        None => return Ok(None),
                    };
                    let Ok(described) = self
                        .cdp_send_one(
                            id,
                            session_id,
                            target_id,
                            CdpMessage {
                                method: "DOM.describeNode".into(),
                                params: Value::Map(vec![
                                    (
                                        Value::Text("nodeId".into()),
                                        Value::Integer(Integer::from(iframe)),
                                    ),
                                    (
                                        Value::Text("depth".into()),
                                        Value::Integer(Integer::from(-1i64)),
                                    ),
                                    (Value::Text("pierce".into()), Value::Bool(true)),
                                ]),
                            },
                            budget_ms,
                        )
                        .await?
                    else {
                        return Ok(None); // app error ⇒ no match (see resolve_locator_node)
                    };
                    match cbor_get(&described, "node")
                        .and_then(|n| cbor_get(n, "contentDocument"))
                        .and_then(|cd| cbor_get(cd, "nodeId"))
                        .and_then(cbor_u64)
                    {
                        Some(n) if n != 0 => root = n,
                        // No in-process contentDocument ⇒ out-of-process (OOPIF)
                        // frame; not handled on this DOM path.
                        _ => return Ok(None),
                    }
                }
                Segment::Css(css) => {
                    match self
                        .dom_query_selector(id, session_id, target_id, root, css, budget_ms)
                        .await?
                    {
                        Some(n) if is_last => return Ok(Some(n)),
                        Some(n) => root = n, // intermediate css scope
                        None => return Ok(None),
                    }
                }
                // text=/role= are resolved by the evaluate-tier resolver, not
                // this DOM path.
                Segment::Text(_) | Segment::Role(_) => return Ok(None),
            }
        }
        // Ended on a `frame=` segment with no leaf target.
        Ok(None)
    }

    /// Resolve a single `text=`/`role=` segment in the top frame's default
    /// execution context: run a marker-attribute resolver (W3C-AccName subset
    /// for `role=`; visible-text match for `text=`), then `querySelector` the
    /// marked node and strip the marker. Returns the matched `nodeId` or `None`.
    async fn resolve_marked_node(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        seg: &Segment,
        budget_ms: u64,
    ) -> Result<Option<u64>, LoomError> {
        use ciborium::value::{Integer, Value};
        let js = match marker_resolver_js(seg) {
            Some(js) => js,
            None => return Ok(None),
        };
        let eval = |expr: String| CdpMessage {
            method: "Runtime.evaluate".into(),
            params: Value::Map(vec![
                (Value::Text("expression".into()), Value::Text(expr)),
                (Value::Text("returnByValue".into()), Value::Bool(true)),
            ]),
        };
        let Ok(resp) = self
            .cdp_send_one(id, session_id, target_id, eval(js), budget_ms)
            .await?
        else {
            return Ok(None); // app error ⇒ no match (see resolve_locator_node)
        };
        let found = cbor_get(&resp, "result")
            .and_then(|r| cbor_get(r, "value"))
            .map(|v| matches!(v, Value::Bool(true)))
            .unwrap_or(false);
        if !found {
            return Ok(None);
        }
        let Ok(doc) = self
            .cdp_send_one(
                id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.getDocument".into(),
                    params: Value::Map(vec![(
                        Value::Text("depth".into()),
                        Value::Integer(Integer::from(0)),
                    )]),
                },
                budget_ms,
            )
            .await?
        else {
            return Ok(None); // app error ⇒ no match (see resolve_locator_node)
        };
        let root = cbor_get(&doc, "root")
            .and_then(|r| cbor_get(r, "nodeId"))
            .and_then(cbor_u64)
            .ok_or_else(|| {
                LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: getDocument: no root.nodeId", id.0),
                )
            })?;
        let node = self
            .dom_query_selector(id, session_id, target_id, root, MARKER_SELECTOR, budget_ms)
            .await?;
        // Best-effort: strip the marker so it does not linger in the DOM.
        let _ = self
            .cdp_send_one(
                id,
                session_id,
                target_id,
                eval(format!(
                    "document.querySelectorAll('{MARKER_SELECTOR}').forEach(function(e){{e.removeAttribute('{MARKER_ATTR}');}})"
                )),
                budget_ms,
            )
            .await;
        Ok(node)
    }

    /// `web.wait` — poll the (possibly `>>`-grammar / `frame=`-prefixed) locator
    /// until it resolves to a node or the deadline elapses. Resolution reuses the
    /// exact host-side path `send_trusted_click` uses (`resolve_locator_node` →
    /// `marker_resolver_js` for `text=`/`role=`), so `web.wait` accepts the SAME
    /// locator grammar as `web.click` — not just a bare CSS selector. A bare
    /// value is treated as CSS (back-compat). `css=` matches presence; `text=` /
    /// `role=` match a VISIBLE element (the same resolver web.click uses).
    ///
    /// `timeout_ms` is the wall-clock deadline (omitted → [`DEFAULT_WAIT_TIMEOUT_MS`],
    /// clamped to [`MAX_WAIT_TIMEOUT_MS`]); the locator is re-probed every
    /// [`WAIT_POLL_INTERVAL_MS`]. Probes are sequential (each awaited before the
    /// next), so they never overlap on the transport. `Resolved` / `PredicateFalse`
    /// are typed application outcomes; a transport failure surfaces as `Err`.
    pub async fn send_wait(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        timeout_ms: Option<u64>,
    ) -> Result<WaitResolveOutcome, LoomError> {
        self.check_breaker(&id)?;

        let deadline_ms = timeout_ms
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
            .min(MAX_WAIT_TIMEOUT_MS);

        // Per-probe CDP budget 0 ⇒ `cdp_send_one` falls back to the configured
        // recv timeout (same as `send_trusted_click`).
        let resolved = poll_locator_until_resolved(
            Duration::from_millis(deadline_ms),
            Duration::from_millis(WAIT_POLL_INTERVAL_MS),
            || self.resolve_locator_node(&id, session_id, target_id, &selector, 0),
        )
        .await;

        match resolved {
            Ok(true) => {
                self.record_success(&id);
                Ok(WaitResolveOutcome::Resolved)
            }
            Ok(false) => {
                // Deadline elapsed without a match — a clean application outcome,
                // NOT a failure (so it must not trip the breaker).
                self.record_success(&id);
                Ok(WaitResolveOutcome::PredicateFalse)
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }
}

/// Poll `probe` every `interval` until it yields `Ok(Some(_))` (→ `Ok(true)`) or
/// `deadline` elapses without a match (→ `Ok(false)`). The first probe runs
/// immediately (an already-present locator resolves with no delay), and the
/// deadline is checked AFTER each miss so a just-in-time appearance still counts.
/// A transport `Err` from a probe aborts the poll (propagated unchanged) — the
/// same fail-fast contract `send_trusted_click` uses for a dead/unreachable shim.
///
/// Time is driven by `tokio::time`, so a `start_paused` test advances the clock
/// virtually (no real sleeping) and the poll/deadline logic stays deterministic.
async fn poll_locator_until_resolved<F, Fut>(
    deadline: Duration,
    interval: Duration,
    mut probe: F,
) -> Result<bool, LoomError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<u64>, LoomError>>,
{
    let start = tokio::time::Instant::now();
    loop {
        if probe().await?.is_some() {
            return Ok(true);
        }
        if start.elapsed() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    // The poll loop is the heart of `send_wait`: it turns the single-probe
    // `resolve_locator_node` into a deadline-bounded wait. These tests pin its
    // three outcomes on a paused clock (no real sleeping), with the probe
    // standing in for the locator resolver.

    /// A locator that appears only on the 3rd probe (the unit-level "delayed
    /// element") still resolves — and the deadline check is AFTER the miss, so
    /// the just-in-time appearance counts.
    #[tokio::test(start_paused = true)]
    async fn poll_resolves_on_delayed_appearance() {
        let calls = Cell::new(0u32);
        let got = poll_locator_until_resolved(
            Duration::from_millis(30_000),
            Duration::from_millis(100),
            || {
                let n = calls.get() + 1;
                calls.set(n);
                // None for the first two probes, Some(node) on the third.
                async move { Ok(if n >= 3 { Some(42u64) } else { None }) }
            },
        )
        .await
        .expect("poll must not error");
        assert!(got, "delayed locator must resolve to true");
        assert_eq!(calls.get(), 3, "should stop probing the moment it resolves");
    }

    /// A locator that never appears polls until the deadline, then reports
    /// `false` (which `send_wait` maps to `PredicateFalse` → `wait_predicate_false`).
    #[tokio::test(start_paused = true)]
    async fn poll_times_out_when_never_resolves() {
        let calls = Cell::new(0u32);
        let got = poll_locator_until_resolved(
            Duration::from_millis(500),
            Duration::from_millis(100),
            || {
                calls.set(calls.get() + 1);
                async { Ok(None) }
            },
        )
        .await
        .expect("a timeout is a clean false, not an error");
        assert!(!got, "a never-appearing locator must time out to false");
        // 500ms / 100ms cadence ⇒ several probes before the deadline trips.
        assert!(
            calls.get() >= 5,
            "should have polled repeatedly before timing out: {}",
            calls.get()
        );
    }

    /// A transport error from a probe aborts the poll immediately (fail-fast,
    /// matching `send_trusted_click`) — it is NOT swallowed into a timeout.
    #[tokio::test(start_paused = true)]
    async fn poll_propagates_transport_error() {
        let err = poll_locator_until_resolved(
            Duration::from_millis(30_000),
            Duration::from_millis(100),
            || async {
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    "shim transport died",
                ))
            },
        )
        .await;
        assert!(err.is_err(), "a probe transport error must propagate");
    }
}
