//! Synthetic `Runtime.evaluate` results, driven by expression sentinels.

use serde_json::{json, Value};

use crate::audio::FAKE_AUDIO_API_OBJECT_ID;

/// Build a synthetic `Runtime.evaluate` response body from a test-only
/// expression sentinel. The fake-chromium does NOT execute JS — it
/// pattern-matches the expression string and constructs the same shape
/// real Chromium would return for the equivalent successful or thrown
/// evaluation.
///
/// Sentinels (all start with `__loom_test_`):
///   `__loom_test_int__`              → result.value = 2 (integer)
///   `__loom_test_str__`              → result.value = "hello"
///   `__loom_test_doc_title__`        → result.value = "My Page"
///   `__loom_test_null__`             → result.value = null
///   `__loom_test_undef__`            → result.type = "undefined", no value
///   `__loom_test_empty_str__`        → result.value = ""
///   `__loom_test_pi__`               → result.value = 3.141592653589793 (float)
///   `__loom_test_obj__`              → result.value = {"label":"Click here","count":42}
///   `__loom_test_throw__:<MSG>`      → exceptionDetails with description Error: <MSG>
///   `__loom_test_large__:<SIZE_KB>`  → result.value = "x" repeated SIZE_KB×1024 times
///   `__loom_test_emit_doc_event__:<STATUS>` → result.value = 1; ALSO emits a
///       Document requestWillBeSent + responseReceived(status=STATUS) BEFORE
///       the response (handled in `Conn::serve` — models a click-
///       triggered link navigation between navigates; stale-event regression)
///
/// Anything else → empty `{}` (caller treats as a no-op evaluate).
pub(crate) fn build_fake_evaluate_response(expression: &str) -> Value {
    // settle-capture: the ReadinessMonitor settle probe (identified by its
    // unique `__loomSettleMut` global) is intercepted in `Conn::serve`
    // BEFORE this function is reached, because its per-tick response is driven
    // by mutable per-connection state (the script index) + the optional
    // `LOOM_FAKE_CHROMIUM_SCRIPT`. See `SettleScript::probe_response`.

    // Throw sentinel:  __loom_test_throw__:<message>
    if let Some(msg) = expression.strip_prefix("__loom_test_throw__:") {
        return json!({
            "result": {
                "type": "object",
                "subtype": "error",
                "className": "Error",
            },
            "exceptionDetails": {
                "exceptionId": 1,
                "text": "Uncaught",
                "lineNumber": 0,
                "columnNumber": 6,
                "scriptId": "1",
                "exception": {
                    "type": "object",
                    "subtype": "error",
                    "className": "Error",
                    "description": format!("Error: {msg}"),
                },
            },
        });
    }

    // Large sentinel:  __loom_test_large__:<size_kb>
    if let Some(rest) = expression.strip_prefix("__loom_test_large__:") {
        let kb: usize = rest.parse().unwrap_or(80);
        // Build a string of EXACT length `kb * 1024 - 2` so the
        // canonical-JSON encoding (which adds two surrounding quotes)
        // is exactly `kb * 1024` bytes. This lets boundary tests target
        // 65_535 / 65_536 / 65_537 byte canonical-JSON outputs precisely.
        let target_chars = kb.saturating_mul(1024).saturating_sub(2);
        let big = "x".repeat(target_chars);
        return json!({
            "result": {
                "type": "string",
                "value": big,
            },
        });
    }

    // Stale-event injection sentinel:  __loom_test_emit_doc_event__:<status>
    // The Document-event emission happens in `Conn::serve` (it needs
    // the websocket writer); here we just return a successful scalar so the
    // evaluate round-trip completes cleanly.
    if expression.starts_with("__loom_test_emit_doc_event__:") {
        return json!({
            "result": { "type": "number", "value": 1 },
        });
    }

    // Bytes-target sentinel:  __loom_test_size__:<bytes>
    // Produces canonical-JSON of exactly <bytes> length (string value
    // wrapped in two quote chars, so the inner string is bytes-2 chars).
    if let Some(rest) = expression.strip_prefix("__loom_test_size__:") {
        let bytes: usize = rest.parse().unwrap_or(65_536);
        let target_chars = bytes.saturating_sub(2);
        let big = "x".repeat(target_chars);
        return json!({
            "result": {
                "type": "string",
                "value": big,
            },
        });
    }

    // voice-call-io task 07: the AudioBridge resolves the per-session nonce'd
    // in-page API object via `Runtime.evaluate window.__loom_<nonce>` (returnByValue
    // false) before every enqueue/startCapture/drain callFunctionOn. Return a stable
    // objectId so the resolve succeeds for any `--audio` session. The api object is
    // "present" regardless of AUDIO_NO_GUM — the missing-mic case (AC10) rejects at
    // `enqueue`, not at resolve.
    if expression.starts_with("window.__loom_") {
        return json!({
            "result": { "type": "object", "objectId": FAKE_AUDIO_API_OBJECT_ID },
        });
    }

    match expression {
        "__loom_test_int__" => json!({
            "result": { "type": "number", "value": 2 },
        }),
        "__loom_test_str__" => json!({
            "result": { "type": "string", "value": "hello" },
        }),
        "__loom_test_doc_title__" => json!({
            "result": { "type": "string", "value": "My Page" },
        }),
        "__loom_test_null__" => json!({
            "result": { "type": "object", "subtype": "null", "value": Value::Null },
        }),
        "__loom_test_undef__" => json!({
            // CDP returns no `value` field for undefined.
            "result": { "type": "undefined" },
        }),
        "__loom_test_empty_str__" => json!({
            "result": { "type": "string", "value": "" },
        }),
        "__loom_test_pi__" => {
            // Real Chromium returns floats verbatim in the value field.
            // Use std::f64::consts::PI to satisfy clippy::approx_constant.
            json!({
                "result": { "type": "number", "value": std::f64::consts::PI },
            })
        }
        "__loom_test_obj__" => json!({
            "result": {
                "type": "object",
                "value": { "label": "Click here", "count": 42 },
            },
        }),
        _ => json!({}),
    }
}
