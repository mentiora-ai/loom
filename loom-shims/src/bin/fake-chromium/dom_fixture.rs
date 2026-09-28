//! `LOOM_FAKE_CHROMIUM_FIXTURE`: the tiny DOM model hit-tests and fills answer from.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// True when the fixture scripts this fill prepare message's ack to be lost:
/// `DOM.resolveNode` for a node whose verdict is `swallow_resolve`, or
/// `Runtime.callFunctionOn` on the `fake-node:` handle of a `swallow_call` node.
pub(crate) fn fill_prepare_ack_swallowed(method: &str, params: &Value) -> bool {
    let (node_id, wanted) = match method {
        "DOM.resolveNode" => (
            params.get("nodeId").and_then(|n| n.as_u64()),
            "swallow_resolve",
        ),
        "Runtime.callFunctionOn" => (
            params
                .get("objectId")
                .and_then(|o| o.as_str())
                .and_then(|o| o.strip_prefix("fake-node:"))
                .and_then(|n| n.parse::<u64>().ok()),
            "swallow_call",
        ),
        _ => return false,
    };
    node_id
        .and_then(|n| dom_fixture().selectors_by_id.get(&n))
        .and_then(|sel| dom_fixture().inputs.get(sel))
        .is_some_and(|verdict| verdict == wanted)
}

/// Tiny DOM model used by the hit-test integration tests. Read once from
/// `LOOM_FAKE_CHROMIUM_FIXTURE` (a path to a JSON file). Empty when
/// unset.
#[derive(Debug, Clone, Default)]
pub(crate) struct DomFixture {
    /// Selector → bounding-box `[x1, y1, x2, y2]` in CSS pixels
    /// (top-left + bottom-right, axis-aligned).
    pub(crate) boxes: HashMap<String, [f64; 4]>,
    /// Selector → assigned synthetic `nodeId`.
    pub(crate) ids_by_selector: HashMap<String, u64>,
    /// Reverse: synthetic `nodeId` → selector.
    pub(crate) selectors_by_id: HashMap<u64, String>,
    /// Viewport `[width, height]` in CSS pixels. Defaults to `[1024, 768]`.
    pub(crate) viewport: [u64; 2],
    /// Selector → scripted `web.type` fill prepare verdict (see module docs).
    pub(crate) inputs: HashMap<String, String>,
    /// `Runtime.releaseObjectGroup` answers with a CDP error.
    pub(crate) release_error: bool,
    /// Delay before answering `DOM.focus`, in milliseconds (0 = none).
    pub(crate) slow_focus_ms: u64,
}

pub(crate) static FIXTURE: OnceLock<DomFixture> = OnceLock::new();

pub(crate) fn dom_fixture() -> &'static DomFixture {
    FIXTURE.get_or_init(load_dom_fixture)
}

pub(crate) fn load_dom_fixture() -> DomFixture {
    let mut out = DomFixture {
        viewport: [1024, 768],
        ..Default::default()
    };
    let path = match std::env::var("LOOM_FAKE_CHROMIUM_FIXTURE") {
        Ok(p) => p,
        Err(_) => return out,
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fake-chromium: cannot read fixture {path}: {e}");
            return out;
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fake-chromium: malformed fixture JSON: {e}");
            return out;
        }
    };
    if let Some(vp) = v.get("viewport").and_then(|x| x.as_array()) {
        if vp.len() == 2 {
            if let (Some(w), Some(h)) = (vp[0].as_u64(), vp[1].as_u64()) {
                out.viewport = [w, h];
            }
        }
    }
    if let Some(inputs) = v.get("inputs").and_then(|i| i.as_object()) {
        for (sel, verdict) in inputs {
            if let Some(verdict) = verdict.as_str() {
                out.inputs.insert(sel.clone(), verdict.to_string());
            }
        }
    }
    out.release_error = v
        .get("release_error")
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    out.slow_focus_ms = v.get("slow_focus_ms").and_then(|r| r.as_u64()).unwrap_or(0);
    if let Some(boxes_obj) = v.get("boxes").and_then(|b| b.as_object()) {
        // Stable id assignment: deterministic ordering by selector string.
        let mut keys: Vec<&String> = boxes_obj.keys().collect();
        keys.sort();
        for (i, sel) in keys.iter().enumerate() {
            let arr = match boxes_obj.get(*sel).and_then(|x| x.as_array()) {
                Some(a) if a.len() == 4 => a,
                _ => continue,
            };
            let coords = match (
                arr[0].as_f64(),
                arr[1].as_f64(),
                arr[2].as_f64(),
                arr[3].as_f64(),
            ) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => [x1, y1, x2, y2],
                _ => continue,
            };
            // Synthetic ids start at 1000 to avoid colliding with the
            // root document nodeId (1).
            let node_id = 1000_u64 + (i as u64);
            out.boxes.insert((*sel).clone(), coords);
            out.ids_by_selector.insert((*sel).clone(), node_id);
            out.selectors_by_id.insert(node_id, (*sel).clone());
        }
    }
    out
}
