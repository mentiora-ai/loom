//! Audio harness: answers the four nonce'd in-page audio API calls.
//!
//! voice-call-io task 07: audio harness env hooks (mirroring
//! LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES), consumed in `audio_call_function_on`:
//!   LOOM_FAKE_CHROMIUM_AUDIO_ECHO=1        — replay injected bytes out of drain
//!                                            (the AC4 fake peer, 16 kHz passthrough).
//!   LOOM_FAKE_CHROMIUM_AUDIO_TONE=<f>:<ms> — emit a canned 48 kHz sine (AC3-shape).
//!   LOOM_FAKE_CHROMIUM_AUDIO_NO_GUM=1      — enqueue rejects no_microphone_request (AC10).

use serde_json::{json, Value};

/// The objectId `build_fake_evaluate_response` hands back for `window.__loom_<nonce>`.
pub(crate) const FAKE_AUDIO_API_OBJECT_ID: &str = "fake-audio-api-1";

/// voice-call-io task 07: answer the four nonce'd in-page audio API calls the
/// `AudioBridge` drives (`this.enqueue(` / `this.startCapture(` / `this.stopCapture(`
/// / `this.drain(`), using env-scripted synthetic data. The fake stays DUMB
/// (Architecture A17): echo replays the injected base64 verbatim, tone emits a canned
/// sine, no-gum rejects. This is NOT a general `Runtime.callFunctionOn` interpreter —
/// any other function declaration returns `None`, so non-audio callFunctionOn (e.g.
/// frame-locator resolution) falls through to the unchanged `canned_response`.
///
/// `echo_b64` is the per-connection echo buffer (set on enqueue, taken on the first
/// drain, then `None` → later drains return an empty `more:false` payload).
pub(crate) fn audio_call_function_on(
    params: &Value,
    echo_b64: &mut Option<String>,
) -> Option<Value> {
    // Scope strictly to calls ON the resolved audio API object — a non-audio
    // `callFunctionOn` (e.g. frame-locator resolution) targets a different objectId
    // and must fall through to `canned_response` unchanged, even if its function text
    // coincidentally contains one of the audio tokens below.
    if params.get("objectId").and_then(|v| v.as_str()) != Some(FAKE_AUDIO_API_OBJECT_ID) {
        return None;
    }
    let decl = params
        .get("functionDeclaration")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let no_gum = env_flag("LOOM_FAKE_CHROMIUM_AUDIO_NO_GUM");
    let echo = env_flag("LOOM_FAKE_CHROMIUM_AUDIO_ECHO");
    let tone = std::env::var("LOOM_FAKE_CHROMIUM_AUDIO_TONE").ok();

    if decl.contains("this.enqueue(") {
        if no_gum {
            // AC10: the page never called getUserMedia → enqueue rejects. The shim's
            // `map_enqueue_exception` maps a description containing `no_microphone_request`
            // to the typed kind.
            return Some(json!({
                "exceptionDetails": {
                    "exception": { "description": "Error: no_microphone_request" }
                }
            }));
        }
        if echo {
            // Stash the injected base64 (arguments[0].value) to replay out of drain.
            *echo_b64 = params
                .pointer("/arguments/0/value")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        // enqueue resolves with the clip duration in seconds; the value is
        // observational only (the shim tolerates its absence) → 0.0.
        return Some(json!({ "result": { "type": "number", "value": 0.0 } }));
    }

    if decl.contains("this.startCapture(") || decl.contains("this.stopCapture(") {
        return Some(json!({ "result": { "value": { "ok": true } } }));
    }

    if decl.contains("this.drain(") {
        // Build the drained payload the shim's `parse_drain` expects. samples_b64 is
        // base64 of little-endian f32 native PCM.
        let (samples_b64, sample_rate, tapped) = if echo {
            match echo_b64.take() {
                Some(b64) => (b64, 16_000u32, 1u32), // 16 kHz → shim resample is passthrough (D1)
                None => (String::new(), 0, 0),       // already drained → empty
            }
        } else if let Some(spec) = tone {
            // `freq:ms` → a canned sine at the native 48 kHz AudioContext rate (D2),
            // so the shim runs a REAL 48k→16k resample end to end.
            match parse_tone_spec(&spec) {
                Some((freq, ms)) => (sine_f32_le_b64(freq, ms, 48_000), 48_000, 1),
                None => (String::new(), 0, 0),
            }
        } else {
            (String::new(), 0, 0)
        };
        return Some(json!({
            "result": { "value": {
                "samples_b64": samples_b64,
                "sample_rate": sample_rate,
                "dropped_frames": 0,
                "tapped_tracks": tapped,
                "injected_leaked": false,
                "buffer_cap_hit": false,
                "more": false,
            }}
        }));
    }

    None
}

/// Env boolean: set and equal to `1`/`true` (case-insensitive).
pub(crate) fn env_flag(key: &str) -> bool {
    matches!(std::env::var(key), Ok(v) if v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Parse a `LOOM_FAKE_CHROMIUM_AUDIO_TONE=<freq>:<ms>` spec into `(freq_hz, ms)`.
pub(crate) fn parse_tone_spec(spec: &str) -> Option<(f64, u64)> {
    let (f, ms) = spec.split_once(':')?;
    Some((f.trim().parse().ok()?, ms.trim().parse().ok()?))
}

/// `rate*ms/1000` samples of `0.5*sin(2π·freq·t)`, little-endian f32, STANDARD base64.
/// Bounded by design (callers use ≤ 100 ms), so a single drain stays well under the
/// shim's per-call byte guard.
pub(crate) fn sine_f32_le_b64(freq: f64, ms: u64, rate: u32) -> String {
    use base64::Engine as _;
    // Bound the allocation so a fat-fingered `AUDIO_TONE=440:99999999` can't OOM/stall
    // CI: saturating math (no wrap) + a 10 s ceiling (well past any real test tone).
    let n = (u64::from(rate).saturating_mul(ms) / 1000).min(u64::from(rate) * 10) as usize;
    let mut bytes = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let s = (0.5 * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32;
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}
