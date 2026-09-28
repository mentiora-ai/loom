//! Host-side intercepts of the trusted-input verbs — `web.click`, `web.type`
//! (`fill` / `keystrokes`), `web.press_key`, `web.wait`: CDP `Input.*` and
//! locator resolution without the WASM guest. Split out of
//! `WasmBridge::dispatch_action_blocking` (each block moved verbatim into an
//! immediately-invoked closure, so its `return`s and `?`s keep their meaning).

use crate::guest_args::*;
use crate::settle_budget::{interaction_dispatch_budget_ms, settle_after_input_dispatch};
use crate::wasm_bridge::WasmBridge;
use crate::wire_receipts::*;
use loom_rpc::host_service_adapter::host_service_adapter::{
    Action, AdapterError as HostAdapterError, Receipt,
};
use std::sync::Arc;

impl WasmBridge {
    /// A receipt when `action` is a host-intercepted input verb, else `None`.
    // Each block was moved verbatim into a closure; its `return`s are the original
    // early exits, so clippy's needless_return on the last one is expected.
    #[allow(clippy::too_many_arguments, clippy::needless_return)]
    pub(crate) fn intercept_input_verbs(
        &self,
        action: &Action,
        session: &Arc<loom_core::session_manager::Session>,
        session_id_str: &str,
        handle: &tokio::runtime::Handle,
        deadline_ms: Option<u64>,
    ) -> Option<Result<Receipt, HostAdapterError>> {
        // cdp-trusted-input: web.click is ALWAYS trusted — host-side CDP
        // `Input.dispatchMouseEvent` at the element hit point (like recording, no
        // guest). web.type `mode:keystrokes` and web.press_key are likewise
        // host-side `Input.dispatchKeyEvent` flows. Real input side effects
        // happen at record time only; the receipt's `outcome_hash` is a constant
        // marker so replay stays structural (NFR-DET-01).
        if let Action::WebClick {
            selector, until, ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let sel = selector.clone();
                let settle_until = until.clone();
                let action_id = session.allocate_action_id();
                let dispatch_t0 = std::time::Instant::now();
                match handle.block_on(host.trusted_click(
                    &sid,
                    &sel,
                    interaction_dispatch_budget_ms(deadline_ms),
                )) {
                    Ok(outcome) => {
                        let dispatch_elapsed_ms = dispatch_t0.elapsed().as_millis() as u64;
                        let dispatched = matches!(
                        outcome,
                        loom_host::shim_manager::InputDispatchOutcome::Ok
                            | loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending
                    );
                        let mut receipt = build_input_dispatch_receipt(
                            action_id,
                            session_id_str,
                            action,
                            outcome,
                        );
                        if dispatched {
                            settle_after_input_dispatch(
                                handle,
                                &host,
                                session,
                                &sid,
                                settle_until.as_deref(),
                                deadline_ms,
                                dispatch_elapsed_ms,
                                &mut receipt,
                            );
                        }
                        return Ok(receipt);
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "click_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }
        if let Action::WebType {
            selector,
            text,
            mode,
            until,
            ..
        } = action
        {
            // cdp-trusted-input: the default `fill` mode (CDP Input.insertText —
            // Playwright fill()) and `keystrokes` are dispatched host-side here;
            // `value` (and any unknown string) falls through to the WASM-guest
            // Runtime.evaluate path. Single source of truth: classify_web_type_mode.
            let dispatch = classify_web_type_mode(mode.as_deref());
            if dispatch != WebTypeDispatch::ValueGuest {
                return Some((|| -> Result<Receipt, HostAdapterError> {
                    let host = Arc::clone(&self.host);
                    let sid = session_id_str.to_string();
                    let sel = selector.clone();
                    let txt = text.clone();
                    let settle_until = until.clone();
                    let action_id = session.allocate_action_id();
                    let dispatch_t0 = std::time::Instant::now();
                    let dispatch_budget = interaction_dispatch_budget_ms(deadline_ms);
                    let outcome = match dispatch {
                        WebTypeDispatch::Fill => {
                            handle.block_on(host.type_fill(&sid, &sel, &txt, dispatch_budget))
                        }
                        WebTypeDispatch::Keystrokes => {
                            handle.block_on(host.type_keystrokes(&sid, &sel, &txt, dispatch_budget))
                        }
                        WebTypeDispatch::ValueGuest => unreachable!("guarded above"),
                    };
                    match outcome {
                        Ok(outcome) => {
                            let dispatch_elapsed_ms = dispatch_t0.elapsed().as_millis() as u64;
                            let dispatched = matches!(
                            outcome,
                            loom_host::shim_manager::InputDispatchOutcome::Ok
                                | loom_host::shim_manager::InputDispatchOutcome::DispatchedAckPending
                        );
                            let mut receipt = build_input_dispatch_receipt(
                                action_id,
                                session_id_str,
                                action,
                                outcome,
                            );
                            if dispatched {
                                settle_after_input_dispatch(
                                    handle,
                                    &host,
                                    session,
                                    &sid,
                                    settle_until.as_deref(),
                                    deadline_ms,
                                    dispatch_elapsed_ms,
                                    &mut receipt,
                                );
                            }
                            return Ok(receipt);
                        }
                        Err(e) => {
                            return Ok(recording_error_receipt(
                                action_id,
                                session_id_str,
                                "type_failed",
                                e.to_string(),
                            ))
                        }
                    }
                })());
            }
        }
        if let Action::WebPressKey {
            key,
            selector,
            modifiers,
            ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let k = key.clone();
                let sel = selector.clone();
                let mods = modifiers.clone().unwrap_or_default();
                let action_id = session.allocate_action_id();
                match handle.block_on(host.press_key(
                    &sid,
                    &k,
                    sel,
                    mods,
                    interaction_dispatch_budget_ms(deadline_ms),
                )) {
                    Ok(outcome) => {
                        return Ok(build_input_dispatch_receipt(
                            action_id,
                            session_id_str,
                            action,
                            outcome,
                        ))
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "press_key_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }

        // web.wait is intercepted host-side (like web.click) so it can resolve the
        // SAME `>>` locator grammar (`text=`/`role=`/`css=`/`frame=`) via
        // `host.wait` → `send_wait` and poll until the locator resolves or
        // `timeout_ms` elapses — instead of the old guest path that passed the raw
        // value to `querySelector` (which threw on `text=`/`role=`). Poll timing is
        // never recorded; only the Resolved/PredicateFalse verdict, so replay stays
        // structural (NFR-DET-01).
        if let Action::WebWait {
            selector,
            timeout_ms,
            ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let sel = selector.clone();
                let to = *timeout_ms;
                let action_id = session.allocate_action_id();
                match handle.block_on(host.wait(&sid, &sel, to)) {
                    Ok(outcome) => {
                        return Ok(build_wait_receipt(
                            action_id,
                            session_id_str,
                            action,
                            outcome,
                        ))
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "wait_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }
        None
    }
}
