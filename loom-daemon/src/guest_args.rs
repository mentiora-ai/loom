//! Guest action payloads for the daemon's `WasmBridge` dispatch path: the CDP /
//! CBOR or raw-JS argument bytes a guest verb runs with (`build_chromium_args`,
//! `build_scroll_expression`) and the `web.type` mode router both dispatch sides
//! share (`classify_web_type_mode`). Split out of `wire_receipts.rs`.

use loom_rpc::host_service_adapter::host_service_adapter::Action;

/// Build a CBOR-encoded `CdpMessage` envelope for the given Web.* action.
/// Returns None for actions that don't have a CDP method mapping yet
/// (caller falls back to the legacy JCS-Action encoding).
///
/// This shape MUST match `loom_shared::shim_protocol::CdpMessage` so
/// `ShimManager::send` can decode the bytes via `ciborium_from_slice`.
/// v0.9.6 helper: convert a `serde_json::Value` (typically a cookie object
/// in `web.set_cookies`'s `source.cookies[]`) into a `ciborium::value::Value`
/// for direct embedding in the CDP CBOR envelope. Returns None for shapes
/// the CDP wire can't represent (e.g. arbitrary nested arrays in places
/// chromium expects scalars) — caller drops the entry rather than the
/// whole batch.
pub(crate) fn serde_json_value_to_cbor(v: serde_json::Value) -> Option<ciborium::value::Value> {
    use ciborium::value::Value;
    use serde_json::Value as J;
    match v {
        J::Null => Some(Value::Null),
        J::Bool(b) => Some(Value::Bool(b)),
        J::String(s) => Some(Value::Text(s)),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Integer(i.into()))
            } else if let Some(u) = n.as_u64() {
                Some(Value::Integer((u as i128).try_into().ok()?))
            } else {
                n.as_f64().map(Value::Float)
            }
        }
        J::Array(arr) => Some(Value::Array(
            arr.into_iter()
                .filter_map(serde_json_value_to_cbor)
                .collect(),
        )),
        J::Object(obj) => Some(Value::Map(
            obj.into_iter()
                .filter_map(|(k, v)| Some((Value::Text(k), serde_json_value_to_cbor(v)?)))
                .collect(),
        )),
    }
}

/// Build the JS expression for `web.scroll`. Targets the viewport
/// (`document.scrollingElement`) when the selector is absent, empty,
/// non-matching, or refers to `body`/`html`/the document element; otherwise
/// scrolls the resolved element. Returns `{x: window.scrollX, y: window.scrollY}`
/// so the post-scroll viewport position can be surfaced on the receipt via the
/// evaluate tier (the guest `scroll_verb` runs this through `evaluate_execute`).
///
/// `selector` is embedded via `serde_json::to_string` — a JSON string literal
/// (e.g. `"body"`) or `null` when absent — so a selector containing `"` or `\`
/// (the only user-controlled input) cannot break out of the JS string. Wrapped
/// in an IIFE so the multi-statement body is a single expression whose value
/// `Runtime.evaluate` returns.
pub(crate) fn build_scroll_expression(
    selector: &Option<String>,
    delta_x: i64,
    delta_y: i64,
) -> String {
    // `null` (no selector) or a JSON string literal like `"body"`.
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "null".to_string());
    format!(
        "(()=>{{const el={sel}?document.querySelector({sel}):null;\
         const box=(!el||el===document.body||el===document.documentElement)\
         ?(document.scrollingElement||document.documentElement):el;\
         box.scrollBy({delta_x},{delta_y});\
         return{{x:window.scrollX,y:window.scrollY}};}})()"
    )
}

/// cdp-trusted-input: how a `web.type` invocation dispatches, by `mode`. The
/// SINGLE source of truth shared by the host-side intercept (`wasm_bridge`) and
/// the value-mode JS builder (`build_chromium_args`), so the two routers never
/// drift (see decisions.md D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebTypeDispatch {
    /// Default (`mode` absent or `"fill"`): host-side CDP `Input.insertText`
    /// (Playwright `fill()` — genuine `isTrusted:true` edit that drives a
    /// framework's `onChange`/react-hook-form state).
    Fill,
    /// `mode:"keystrokes"`: host-side per-char `Input.dispatchKeyEvent`.
    Keystrokes,
    /// `mode:"value"` (or any unrecognized string → back-compat): WASM-guest
    /// `Runtime.evaluate` prototype-setter + synthetic `input`/`change` events.
    ValueGuest,
}

/// Classify a `web.type` `mode` into its dispatch path. `None`/`"fill"` → `Fill`
/// (the default after the flip); `"keystrokes"` → `Keystrokes`; everything else
/// — `"value"` AND any unknown string — → `ValueGuest`, preserving the pre-flip
/// "unknown → value" behavior (decisions.md D5/D8; council: don't error on an
/// unknown mode).
pub(crate) fn classify_web_type_mode(mode: Option<&str>) -> WebTypeDispatch {
    match mode {
        None | Some("fill") => WebTypeDispatch::Fill,
        Some("keystrokes") => WebTypeDispatch::Keystrokes,
        _ => WebTypeDispatch::ValueGuest,
    }
}

pub(crate) fn build_chromium_args(action: &Action) -> Option<Vec<u8>> {
    use ciborium::value::Value;

    // Build the `Runtime.evaluate` envelope for selector-driven verbs.
    // `expression` is built from CLI-provided strings (selector / text /
    // value), so we MUST embed them as JSON-encoded string literals
    // (`serde_json::to_string`) — naive `format!("{s}")` would let
    // a `"` in the selector break out of the JS string.
    let runtime_evaluate = |expression: String| -> Value {
        Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Runtime.evaluate".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![
                    (Value::Text("expression".into()), Value::Text(expression)),
                    (Value::Text("returnByValue".into()), Value::Bool(true)),
                    (Value::Text("awaitPromise".into()), Value::Bool(false)),
                ]),
            ),
        ])
    };

    let msg = match action {
        Action::WebNavigate { url, .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Page.navigate".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![
                    (Value::Text("url".into()), Value::Text(url.clone())),
                    (
                        Value::Text("transitionType".into()),
                        Value::Text("typed".into()),
                    ),
                ]),
            ),
        ]),

        // cdp-trusted-input: web.click is ALWAYS trusted now — intercepted
        // host-side (CDP Input.dispatchMouseEvent at the element hit point),
        // like recording. No guest Runtime.evaluate args. Handled in wasm_bridge
        // before build_chromium_args is reached; this arm satisfies the match.
        Action::WebClick { .. } => return None,

        Action::WebEvaluate { expression, .. } => runtime_evaluate(expression.clone()),

        // web.set_input_files uses the typed `set_input_files_execute` host
        // function (like navigate/evaluate), NOT a single CDP envelope —
        // `args_canonical_bytes` special-cases it before this fn is called.
        // This arm exists only to satisfy the exhaustive match.
        Action::WebSetInputFiles { .. } => return None,
        // Intercepted before build_chromium_args (host/shim read, no CDP).
        Action::WebNetworkLog { .. } => return None,
        // video-capture: recording is a streaming flow (start → frames → stop),
        // not a single CDP command, so it has no direct-CDP args envelope here —
        // handled by the surface `start-recording`/`stop-recording` verbs.
        Action::WebStartRecording { .. } | Action::WebStopRecording { .. } => return None,
        // cdp-trusted-input: web.press_key is a host-side CDP Input.* verb (no
        // guest); intercepted in wasm_bridge before build_chromium_args.
        Action::WebPressKey { .. } => return None,
        // voice-call-io: audio verbs are host-side intercepts (no direct-CDP
        // envelope), like network_log/recording — intercepted in wasm_bridge before
        // build_chromium_args, so they have no CDP-replay args (None). (`WebSay` is
        // task 09; it too resolves to InjectAudio host-side.)
        Action::WebInjectAudio { .. }
        | Action::WebStartAudioCapture { .. }
        | Action::WebStopAudioCapture { .. }
        | Action::WebSay { .. } => return None,

        Action::WebType {
            selector,
            text,
            mode,
            ..
        } => {
            // cdp-trusted-input: the default `fill` mode and `keystrokes` are
            // intercepted host-side in wasm_bridge (CDP Input.insertText /
            // dispatchKeyEvent); only the legacy `value` mode (and any unknown
            // string → value, back-compat) builds the Runtime.evaluate args here.
            // Single source of truth: classify_web_type_mode (decisions.md D8).
            if classify_web_type_mode(mode.as_deref()) != WebTypeDispatch::ValueGuest {
                return None;
            }
            // Direct `el.value = text` bypasses React/Vue/Angular value
            // trackers — the DOM `.value` is set but framework state still
            // thinks the field is empty, so a follow-up form submit fails
            // with "this field is required". Use the native prototype
            // setter so the framework's tracker fires its change observer.
            // (Same approach Playwright/testing-library use for the same
            // reason.)
            let sel = serde_json::to_string(selector).ok()?;
            let val = serde_json::to_string(text).ok()?;
            runtime_evaluate(format!(
                "(function(){{\
                  const el=document.querySelector({sel});\
                  el.focus();\
                  const proto=el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;\
                  const setter=Object.getOwnPropertyDescriptor(proto,'value').set;\
                  setter.call(el,{val});\
                  el.dispatchEvent(new Event('input',{{bubbles:true}}));\
                  el.dispatchEvent(new Event('change',{{bubbles:true}}));\
                }})()"
            ))
        }

        Action::WebSelect {
            selector, value, ..
        } => {
            // Same React/Vue/Angular tracker problem as web.type — the
            // native HTMLSelectElement setter is what frameworks observe.
            let sel = serde_json::to_string(selector).ok()?;
            let val = serde_json::to_string(value).ok()?;
            runtime_evaluate(format!(
                "(function(){{\
                  const el=document.querySelector({sel});\
                  const setter=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set;\
                  setter.call(el,{val});\
                  el.dispatchEvent(new Event('input',{{bubbles:true}}));\
                  el.dispatchEvent(new Event('change',{{bubbles:true}}));\
                }})()"
            ))
        }

        Action::WebHover { selector, .. } => {
            let sel = serde_json::to_string(selector).ok()?;
            runtime_evaluate(format!(
                "document.querySelector({sel}).dispatchEvent(\
                 new MouseEvent('mouseover',{{bubbles:true,cancelable:true}}))"
            ))
        }

        Action::WebScroll {
            selector,
            delta_x,
            delta_y,
            ..
        } => {
            // Dead-for-dispatch: scroll uses the raw-expression `args_canonical_bytes`
            // path (like WebEvaluate), not this CBOR envelope. Kept for the
            // `build_chromium_args_*` tests + parity with the WebEvaluate arm.
            // Single source of the scroll JS = `build_scroll_expression`.
            runtime_evaluate(build_scroll_expression(
                selector,
                delta_x.unwrap_or(0),
                delta_y.unwrap_or(0),
            ))
        }

        // web.wait is now intercepted host-side (like web.click): the daemon polls
        // the locator via `host.wait` → `send_wait` (reusing the `resolve_locator_node`
        // grammar resolver), so there is no guest Runtime.evaluate envelope. Handled
        // in wasm_bridge before build_chromium_args is reached; this arm satisfies
        // the match. (The old raw `querySelector(sel)` probe threw on text=/role=.)
        Action::WebWait { .. } => return None,

        // settle-capture: web.wait_for uses the typed `wait_for_execute` host
        // function (like navigate/evaluate), NOT a single CDP envelope —
        // `args_canonical_bytes` special-cases it before this fn is called.
        // This arm exists only to satisfy the exhaustive match.
        Action::WebWaitFor { .. } => return None,

        Action::WebScreenshot { .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Page.captureScreenshot".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![(
                    Value::Text("format".into()),
                    Value::Text("png".into()),
                )]),
            ),
        ]),

        Action::WebSnapshot { .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("DOM.getDocument".into()),
            ),
            (
                Value::Text("params".into()),
                Value::Map(vec![
                    (
                        Value::Text("depth".into()),
                        Value::Integer((-1i128).try_into().ok()?),
                    ),
                    // pierce:true inlines shadow-DOM + iframe contentDocument subtrees,
                    // matching web.navigate (shim STEP 5) so the two DOM captures hash a
                    // comparable node set. Normalized via dom_normalize (frameId stripped
                    // recursively) at the shim cdp_send chokepoint.
                    (Value::Text("pierce".into()), Value::Bool(true)),
                ]),
            ),
        ]),

        // v0.9.6 web-cookie-injection: build CDP envelopes daemon-side
        // (the WASM verbs in loom-surface-web forward whatever
        // action.payload they receive via host::shim_call, so we need
        // the payload to be a valid CDP CBOR envelope by the time it
        // reaches the chromium shim).
        //
        // Per-cookie validation (validate_cookie_params) is intentionally
        // NOT performed on this raw-JSON `source` path — it would require
        // converting the untyped `source` into typed `loom_shared::cookie_types`
        // structs first. The inline path validates via the typed
        // `validate_cookie_params` above; on this path the chromium shim's
        // Network.setCookies response surfaces individual cookie rejections.
        //
        // Grant resolution is now performed upstream in
        // `dispatch_action_blocking` (v0.9.7 follow-up A) — by the
        // time we get here the source should always be `inline`.
        // The non-inline branch below is defensive: if some other
        // caller (e.g. tests) hands us a `grant` source we emit an
        // empty no-op envelope rather than trapping.
        Action::WebSetCookies { source, .. } => {
            // source = {"source":"inline","cookies":[...]} or
            // {"source":"grant","grant_id":"..."}
            let kind = source.get("source").and_then(|v| v.as_str())?;
            if kind != "inline" {
                tracing::warn!(
                    "build_chromium_args saw set_cookies with non-inline source after dispatcher should have resolved it; emitting empty Network.setCookies",
                );
                return Some({
                    let v = Value::Map(vec![
                        (
                            Value::Text("method".into()),
                            Value::Text("Network.setCookies".into()),
                        ),
                        (
                            Value::Text("params".into()),
                            Value::Map(vec![(Value::Text("cookies".into()), Value::Array(vec![]))]),
                        ),
                    ]);
                    let mut bytes = Vec::new();
                    ciborium::ser::into_writer(&v, &mut bytes).ok()?;
                    bytes
                });
            }
            let cookies = source
                .get("cookies")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| serde_json_value_to_cbor(c.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Value::Map(vec![
                (
                    Value::Text("method".into()),
                    Value::Text("Network.setCookies".into()),
                ),
                (
                    Value::Text("params".into()),
                    Value::Map(vec![(Value::Text("cookies".into()), Value::Array(cookies))]),
                ),
            ])
        }
        Action::WebGetCookies { urls, .. } => {
            let mut params: Vec<(Value, Value)> = vec![];
            if let Some(u) = urls {
                params.push((
                    Value::Text("urls".into()),
                    Value::Array(u.iter().map(|s| Value::Text(s.clone())).collect()),
                ));
            }
            Value::Map(vec![
                (
                    Value::Text("method".into()),
                    Value::Text("Network.getCookies".into()),
                ),
                (Value::Text("params".into()), Value::Map(params)),
            ])
        }
        Action::WebClearCookies { .. } => Value::Map(vec![
            (
                Value::Text("method".into()),
                Value::Text("Network.clearBrowserCookies".into()),
            ),
            (Value::Text("params".into()), Value::Map(vec![])),
        ]),
        Action::WebDeleteCookies {
            name,
            url,
            domain,
            path,
            ..
        } => {
            let mut params: Vec<(Value, Value)> =
                vec![(Value::Text("name".into()), Value::Text(name.clone()))];
            if let Some(u) = url {
                params.push((Value::Text("url".into()), Value::Text(u.clone())));
            }
            if let Some(d) = domain {
                params.push((Value::Text("domain".into()), Value::Text(d.clone())));
            }
            if let Some(p) = path {
                params.push((Value::Text("path".into()), Value::Text(p.clone())));
            }
            Value::Map(vec![
                (
                    Value::Text("method".into()),
                    Value::Text("Network.deleteCookies".into()),
                ),
                (Value::Text("params".into()), Value::Map(params)),
            ])
        }
    };

    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&msg, &mut bytes).ok()?;
    Some(bytes)
}
