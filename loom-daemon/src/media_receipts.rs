//! Recording and voice-call audio receipts (screencast start/stop, inject/say,
//! capture start/stop) and the inject-error classifier. Split out of
//! `wire_receipts.rs`.

use crate::wire_receipts::build_recording_started_receipt;
use crate::wire_receipts::recording_error_receipt;
use loom_rpc::host_service_adapter::host_service_adapter::Receipt;

/// video-capture: `web.stop_recording` receipt. On success carries
/// `screencast_after_hash` (the `.webm` CAS hash). A best-effort encode failure
/// (encoder unavailable / zero frames) → an error receipt whose detail carries
/// the `stop_reason` + `error` so the agent gets an actionable message; the
/// session itself was never aborted.
pub(crate) fn build_stop_recording_receipt(
    action_id: u64,
    session_id: &str,
    result: loom_host::wasm_host::wasm_host::ScreencastResult,
) -> Receipt {
    match result.screencast_after_hash {
        Some(hash) => {
            let mut r = build_recording_started_receipt(action_id, session_id);
            r.screencast_after_hash = Some(hash);
            r
        }
        None => recording_error_receipt(
            action_id,
            session_id,
            "recording_failed",
            result
                .error
                .unwrap_or_else(|| format!("recording produced no video ({})", result.stop_reason)),
        ),
    }
}

/// voice-call-io (task 04): success receipt for `web.inject_audio`. Carries a
/// CONSTANT per-verb `outcome_hash` dispatch-success marker (NOT audio/page state),
/// exactly like the interaction verbs (`build_input_dispatch_receipt`) and
/// `build_wait_receipt`, so a voice session's manifest hash chain stays
/// replay-equal. `await_playout` completion is surfaced only as a daemon-side
/// tracing event (D18), never on the receipt or in the hash.
pub(crate) fn build_inject_audio_receipt(action_id: u64, session_id: &str) -> Receipt {
    let mut r = build_recording_started_receipt(action_id, session_id);
    r.outcome_hash = Some(loom_core::content_store::sha256_hex(
        b"loom:audio:inject-ok",
    ));
    r
}

/// voice-call-io (task 09): success receipt for `web.say`. TTS output is injected
/// through the ordinary `inject_audio` path, so this carries the SAME constant
/// enqueue-success `outcome_hash` shape as `build_inject_audio_receipt` (a distinct
/// per-verb marker keeps the manifest hash chain replay-equal; playout completion is
/// a daemon tracing event, never a receipt field — D18). Replay-exclusion is inherited
/// from the inject path — `web.say` adds no new replay wiring.
pub(crate) fn build_say_receipt(action_id: u64, session_id: &str) -> Receipt {
    let mut r = build_recording_started_receipt(action_id, session_id);
    r.outcome_hash = Some(loom_core::content_store::sha256_hex(b"loom:audio:say-ok"));
    r
}

/// voice-call-io (task 06): success receipt for `web.start_audio_capture` (capture
/// began; the WAV hash arrives on the stop receipt). Carries a CONSTANT per-verb
/// `outcome_hash` dispatch marker (PRD D6), mirroring `build_inject_audio_receipt`.
pub(crate) fn build_audio_capture_started_receipt(action_id: u64, session_id: &str) -> Receipt {
    let mut r = build_recording_started_receipt(action_id, session_id);
    r.outcome_hash = Some(loom_core::content_store::sha256_hex(
        b"loom:audio:capture-start-ok",
    ));
    r
}

/// voice-call-io (task 06): `web.stop_audio_capture` receipt. On success carries the
/// observational `audio_after_hash` (the captured `.wav` CAS hash), a CONSTANT
/// per-verb `outcome_hash`, and `audio_stop_reason` (so a `byte_cap`/`duration_cap`
/// truncation is caller-observable, not just a server log). The WAV bytes live in
/// CAS OUTSIDE the manifest hash chain (host-intercept, like screencast), so capture
/// never affects replay-equality. A capture that produced no audio (no inbound track
/// / mux error / host over-ceiling reject) → an error receipt carrying the
/// stop_reason + error; the session itself is never aborted.
pub(crate) fn build_stop_audio_capture_receipt(
    action_id: u64,
    session_id: &str,
    result: loom_host::wasm_host::wasm_host::AudioCaptureResult,
) -> Receipt {
    match result.audio_after_hash {
        Some(hash) => {
            let mut r = build_recording_started_receipt(action_id, session_id);
            r.outcome_hash = Some(loom_core::content_store::sha256_hex(
                b"loom:audio:capture-stop-ok",
            ));
            r.audio_after_hash = Some(hash);
            r.audio_stop_reason = Some(result.stop_reason);
            r
        }
        None => {
            // Preserve the typed stop_reason on the error receipt too (C4/#18):
            // a caller seeing `no_inbound_track` / `no_samples` / `session_closed`
            // gets the reason, not only a free-text message.
            let stop_reason = result.stop_reason.clone();
            let mut r = recording_error_receipt(
                action_id,
                session_id,
                "audio_capture_failed",
                result
                    .error
                    .unwrap_or_else(|| format!("capture produced no audio ({stop_reason})")),
            );
            r.audio_stop_reason = Some(stop_reason);
            r
        }
    }
}

/// Map a failed `web.inject_audio` (the `LoomError` message threaded up from the
/// shim's typed `detail`) to a typed receipt `error.kind`. The shim emits the bare
/// kind in `ShimResponse::Error.detail`; it arrives here embedded in the host error
/// string (`"shim chromium:<sid>: <kind>"`), so match on substrings. Unknown →
/// `inject_failed` (never silently succeeds).
pub(crate) fn classify_inject_error(message: &str) -> &'static str {
    for kind in [
        "no_microphone_request",
        "audio_decode_failed",
        "audio_not_enabled",
        "inject_timeout",
        "audio_bridge_unavailable",
        "payload_too_large",
        "invalid_argument",
        "blob_not_found",
        "determinism_enabled",
    ] {
        if message.contains(kind) {
            return kind;
        }
    }
    "inject_failed"
}

#[cfg(test)]
mod audio_capture_receipt_tests {
    use super::*;
    use loom_host::wasm_host::wasm_host::AudioCaptureResult;
    use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;

    fn constant(marker: &[u8]) -> String {
        loom_core::content_store::sha256_hex(marker)
    }

    #[test]
    fn stop_success_carries_hash_reason_and_constant_marker() {
        let hash = "ab".repeat(32);
        let result = AudioCaptureResult {
            audio_after_hash: Some(hash.clone()),
            sample_count: 16_000,
            duration_ms: 1_000,
            dropped_frames: 0,
            source_sample_rate: 48_000,
            stop_reason: "explicit".to_string(),
            error: None,
        };
        let r = build_stop_audio_capture_receipt(7, "sess-1", result);
        assert!(matches!(r.status, ReceiptStatus::Success));
        assert_eq!(r.audio_after_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(r.audio_stop_reason.as_deref(), Some("explicit"));
        // Constant per-verb dispatch marker (PRD D6) — NOT the audio bytes hash, so
        // two different captures chain identically at the manifest layer.
        assert_eq!(
            r.outcome_hash,
            Some(constant(b"loom:audio:capture-stop-ok"))
        );
        assert_ne!(r.outcome_hash.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn stop_surfaces_cap_truncation_reason_to_caller() {
        // C4: a byte_cap/duration_cap truncation is caller-observable on the receipt,
        // not just a server log line.
        let result = AudioCaptureResult {
            audio_after_hash: Some("cd".repeat(32)),
            stop_reason: "byte_cap".to_string(),
            ..Default::default()
        };
        let r = build_stop_audio_capture_receipt(1, "s", result);
        assert_eq!(r.audio_stop_reason.as_deref(), Some("byte_cap"));
    }

    #[test]
    fn stop_with_no_audio_is_typed_error_not_a_kill() {
        // M18: a capture that produced no audio → Error-status receipt (session is
        // never aborted); no audio hash.
        let result = AudioCaptureResult {
            audio_after_hash: None,
            stop_reason: "no_inbound_track".to_string(),
            error: Some("no inbound audio track".to_string()),
            ..Default::default()
        };
        let r = build_stop_audio_capture_receipt(1, "s", result);
        assert!(matches!(r.status, ReceiptStatus::Error));
        assert!(r.audio_after_hash.is_none());
        assert!(r.error.is_some());
    }

    #[test]
    fn markers_are_stable_and_distinct() {
        // M8: start/stop each carry a stable, distinct constant marker across calls.
        let s1 = build_audio_capture_started_receipt(1, "a").outcome_hash;
        let s2 = build_audio_capture_started_receipt(2, "b").outcome_hash;
        assert_eq!(s1, s2, "start marker must be constant across captures");
        assert_eq!(s1, Some(constant(b"loom:audio:capture-start-ok")));
        assert_ne!(
            s1,
            Some(constant(b"loom:audio:capture-stop-ok")),
            "start marker must differ from stop marker"
        );
    }
}
