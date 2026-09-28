//! Stateless canned CDP responses.

use serde_json::{json, Value};

use crate::dom_fixture::dom_fixture;

/// Local copy of the browser-scope classifier so fake-chromium doesn't
/// need to depend on loom-shims' library code (it's a separate bin
/// target). Stays in sync with `is_browser_scope_method` in
/// `loom-shims/src/cdp_connection/interfaces.rs`.
pub(crate) fn is_browser_scope_method_local(method: &str) -> bool {
    matches!(
        method.split('.').next().unwrap_or(""),
        "Browser" | "Target" | "Tracing" | "Storage" | "Schema" | "SystemInfo" | "Memory"
    )
}

/// Canned CDP-method responses. Best-effort — unknown methods get an
/// empty `{}` result so the daemon doesn't error out on routine calls.
///
/// `params` is consumed for `DOM.querySelector` (extract `selector`) and
/// `DOM.getBoxModel` (extract `nodeId`); for other methods it is ignored.
/// A per-process, per-call ephemeral frame id — stands in for the random
/// per-navigation `frameId` real Chromium embeds in `DOM.getDocument`. Distinct
/// across independent runs so a content-stable `dom_snapshot_hash` can only hold
/// if the shim strips it.
pub(crate) fn ephemeral_frame_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "fake-frame-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn canned_response(method: &str, params: &Value) -> Value {
    match method {
        "Page.navigate" => json!({
            "frameId": "fake-frame-1",
            "loaderId": "fake-loader-1"
        }),
        "DOM.getDocument" => {
            // Honor `pierce`: real Chromium inlines shadow-DOM + iframe
            // contentDocument subtrees only when pierce:true. Each inlined document
            // carries its OWN ephemeral frameId. Node ids are STABLE synthetic
            // values; only the frameIds vary per call, so two captures of the same
            // tree normalize (frameId stripped recursively) to identical bytes —
            // which the pierced-path determinism e2e asserts.
            //
            // This fixture validates the normalization plumbing ONLY. It does NOT
            // reproduce browser-enforced same-origin / CORS isolation that real
            // Chromium applies to pierced subtrees.
            let pierce = params
                .get("pierce")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            if pierce {
                json!({
                    "root": {
                        "nodeId": 1,
                        "backendNodeId": 1,
                        "nodeName": "#document",
                        "nodeType": 9,
                        "childNodeCount": 2,
                        "frameId": ephemeral_frame_id(),
                        "children": [
                            // Shadow host — its shadowRoot subtree is inlined under pierce.
                            {
                                "nodeId": 2,
                                "backendNodeId": 2,
                                "nodeName": "DIV",
                                "nodeType": 1,
                                "shadowRoots": [
                                    {
                                        "nodeId": 3,
                                        "backendNodeId": 3,
                                        "nodeName": "#document-fragment",
                                        "nodeType": 11,
                                        "children": [
                                            {
                                                "nodeId": 4,
                                                "backendNodeId": 4,
                                                "nodeName": "SPAN",
                                                "nodeType": 1,
                                                "children": []
                                            }
                                        ]
                                    }
                                ],
                                "children": []
                            },
                            // Iframe — its contentDocument is inlined under pierce,
                            // each level carrying its own ephemeral frameId.
                            {
                                "nodeId": 5,
                                "backendNodeId": 5,
                                "nodeName": "IFRAME",
                                "nodeType": 1,
                                "frameId": ephemeral_frame_id(),
                                "contentDocument": {
                                    "nodeId": 6,
                                    "backendNodeId": 6,
                                    "nodeName": "#document",
                                    "nodeType": 9,
                                    "frameId": ephemeral_frame_id(),
                                    "children": [
                                        {
                                            "nodeId": 7,
                                            "backendNodeId": 7,
                                            "nodeName": "BODY",
                                            "nodeType": 1,
                                            "children": []
                                        }
                                    ]
                                }
                            }
                        ]
                    }
                })
            } else {
                json!({
                    "root": {
                        "nodeId": 1,
                        "backendNodeId": 1,
                        "nodeName": "#document",
                        "nodeType": 9,
                        "childNodeCount": 0,
                        // Ephemeral per-navigation frame id, mirroring real Chromium.
                        // Varies per call + per process so the determinism e2e proves
                        // `dom_snapshot_hash` normalization STRIPS it: two independent
                        // same-seed runs hash identically ONLY because the shim removes
                        // this id (see loom_shared::dom_normalize).
                        "frameId": ephemeral_frame_id(),
                        "children": []
                    }
                })
            }
        }
        "DOM.querySelector" => {
            let sel = params
                .get("selector")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let node_id = dom_fixture().ids_by_selector.get(sel).copied().unwrap_or(0);
            json!({ "nodeId": node_id })
        }
        "DOM.scrollIntoViewIfNeeded" => json!({}),
        // web.type fill prepare step (fixture `inputs`): resolveNode → a
        // `fake-node:<nodeId>` handle; callFunctionOn on it → the scripted verdict.
        "DOM.resolveNode" => {
            let node_id = params.get("nodeId").and_then(|n| n.as_u64()).unwrap_or(0);
            match dom_fixture().selectors_by_id.get(&node_id) {
                None => json!({
                    "__cdp_error__": { "code": -32000, "message": "No node with given id found" }
                }),
                Some(sel) => match dom_fixture().inputs.get(sel).map(String::as_str) {
                    Some("resolve_error") => json!({
                        "__cdp_error__": { "code": -32000, "message": "Node is not resolvable" }
                    }),
                    Some("no_object") => json!({ "object": { "type": "undefined" } }),
                    _ => json!({
                        "object": {
                            "type": "object",
                            "subtype": "node",
                            "objectId": format!("fake-node:{node_id}"),
                        }
                    }),
                },
            }
        }
        "Runtime.callFunctionOn" => {
            let node_id = params
                .get("objectId")
                .and_then(|o| o.as_str())
                .and_then(|o| o.strip_prefix("fake-node:"))
                .and_then(|n| n.parse::<u64>().ok());
            let verdict = node_id
                .and_then(|n| dom_fixture().selectors_by_id.get(&n))
                .and_then(|sel| dom_fixture().inputs.get(sel))
                .map(String::as_str)
                .unwrap_or("insert");
            let by_value = |value: Value| json!({ "result": { "type": "object", "value": value } });
            match verdict {
                "throw" => json!({
                    "result": { "type": "object", "subtype": "error" },
                    "exceptionDetails": {
                        "text": "Uncaught",
                        "exception": { "description": "Error: fake page exception" }
                    }
                }),
                "garbage" => by_value(json!({ "v": "sideways" })),
                other => match other.strip_prefix("malformed:") {
                    Some(input_type) => by_value(json!({ "v": "malformed", "type": input_type })),
                    None => by_value(json!({ "v": other })),
                },
            }
        }
        "Runtime.releaseObjectGroup" => {
            if dom_fixture().release_error {
                json!({ "__cdp_error__": { "code": -32000, "message": "fake release failure" } })
            } else {
                json!({})
            }
        }
        // cdp-trusted-input: focus + real CDP input dispatch. The fake doesn't
        // model keyboard/mouse state — it just acks (empty success), which is
        // enough to assert the host issues the right CDP envelopes and records
        // the trusted-input receipt.
        "DOM.focus" => json!({}),
        "Input.dispatchKeyEvent" => json!({}),
        "Input.insertText" => json!({}),
        "Input.dispatchMouseEvent" => json!({}),
        "DOM.getBoxModel" => {
            let nid = params.get("nodeId").and_then(|n| n.as_u64()).unwrap_or(0);
            let fixture = dom_fixture();
            match fixture
                .selectors_by_id
                .get(&nid)
                .and_then(|sel| fixture.boxes.get(sel))
            {
                Some(b) => {
                    let [x1, y1, x2, y2] = *b;
                    if (x2 - x1) <= 0.0 || (y2 - y1) <= 0.0 {
                        // Real Chromium returns -32000 for zero-area /
                        // hidden / detached elements. The hit_test helper
                        // maps any DOM.getBoxModel error to
                        // ShimFailureKind::HitTestFailed.
                        return json!({
                            "__cdp_error__": {
                                "code": -32000,
                                "message": "Could not compute box model.",
                            }
                        });
                    }
                    let w = (x2 - x1) as u64;
                    let h = (y2 - y1) as u64;
                    json!({
                        "model": {
                            "content": [x1, y1, x2, y1, x2, y2, x1, y2],
                            "padding": [x1, y1, x2, y1, x2, y2, x1, y2],
                            "border":  [x1, y1, x2, y1, x2, y2, x1, y2],
                            "margin":  [x1, y1, x2, y1, x2, y2, x1, y2],
                            "width":  w,
                            "height": h,
                        }
                    })
                }
                None => json!({
                    "__cdp_error__": {
                        "code": -32000,
                        "message": "Could not find node with given id",
                    }
                }),
            }
        }
        "Page.getLayoutMetrics" => {
            let [w, h] = dom_fixture().viewport;
            json!({
                "layoutViewport":   { "pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": h },
                "visualViewport":   { "offsetX": 0.0, "offsetY": 0.0, "pageX": 0.0, "pageY": 0.0, "clientWidth": w, "clientHeight": h, "scale": 1.0, "zoom": 1.0 },
                "cssLayoutViewport":{ "pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": h },
                "contentSize":      { "x": 0.0, "y": 0.0, "width": w, "height": h },
                "cssContentSize":   { "x": 0.0, "y": 0.0, "width": w, "height": h },
            })
        }
        "Page.captureScreenshot" => json!({
            // 1×1 transparent PNG, base64-encoded.
            "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII="
        }),
        "Page.addScriptToEvaluateOnNewDocument" => json!({
            "identifier": "1"
        }),
        "Target.createTarget" => json!({
            "targetId": "fake-target-1"
        }),
        "Target.attachToTarget" => json!({
            // Stable id so integration tests can assert echo-back.
            "sessionId": "fake-session-1"
        }),
        "Page.enable" => json!({}),
        "Network.enable" => json!({}),
        "DOM.enable" => json!({}),
        "Runtime.enable" => json!({}),
        // Fetch domain methods used by
        // the network_interceptor's blocklist gate. All return empty
        // success per CDP convention.
        "Fetch.enable" => json!({}),
        "Fetch.continueRequest" => json!({}),
        "Fetch.failRequest" => json!({}),
        "Target.getTargets" => json!({
            "targetInfos": []
        }),
        // Page.enable, Network.enable, DOM.enable, etc.
        _ => json!({}),
    }
}
