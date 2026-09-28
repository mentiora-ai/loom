//! Host-side intercepts of the media verbs — `web.network_log`, screencast
//! recording, voice-call audio (inject / capture / say): dispatched against the
//! shim without the WASM guest. Split out of `WasmBridge::dispatch_action_blocking`
//! (each block moved verbatim into an immediately-invoked closure, so its
//! `return`s and `?`s keep their meaning).

use crate::inject_payload::{resolve_inject_payload, MAX_INJECT_BYTES};
use crate::map_loom_error;
use crate::media_receipts::*;
use crate::wasm_bridge::WasmBridge;
use crate::wire_receipts::*;
use loom_rpc::host_service_adapter::host_service_adapter::{
    Action, AdapterError as HostAdapterError, Receipt,
};
use std::sync::Arc;

impl WasmBridge {
    /// A receipt when `action` is a host-intercepted media verb, else `None`.
    // Each block was moved verbatim into a closure; its `return`s are the original
    // early exits, so clippy's needless_return on the last one is expected.
    #[allow(clippy::too_many_arguments, clippy::needless_return)]
    pub(crate) fn intercept_media_verbs(
        &self,
        action: &Action,
        session: &Arc<loom_core::session_manager::Session>,
        session_id_str: &str,
        handle: &tokio::runtime::Handle,
        _deadline_ms: Option<u64>,
    ) -> Option<Result<Receipt, HostAdapterError>> {
        // web.network_log is a host/shim READ — it does NOT run the WASM guest,
        // navigate, or touch the replay hash chain. Intercept here and build the
        // receipt directly from the shim accumulator.
        if let Action::WebNetworkLog { .. } = action {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let action_id = session.allocate_action_id();
                // Plain block_on: we're on a spawn_blocking thread (see the
                // WasmHostBridge threading contract) — block_in_place would
                // panic here, and isn't needed off the worker pool.
                let data = handle.block_on(host.network_log(&sid)).map_err(|e| {
                    tracing::error!(
                        session_id = %sid,
                        error = %e,
                        "web.network_log failed"
                    );
                    map_loom_error(&e)
                })?;
                return Ok(build_network_log_receipt(action_id, session_id_str, data));
            })());
        }

        // video-capture: web.start_recording / web.stop_recording are
        // host/shim-side streaming actions (CDP Page.startScreencast + an ffmpeg
        // encode) — they do NOT run the WASM guest, navigate, or touch the
        // replay hash chain. Intercept here, exactly like web.network_log. The
        // recorded .webm bytes live in CAS OUTSIDE the chain (only the content
        // hash is returned), so recording never affects replay-equality.
        if let Action::WebStartRecording {
            max_duration_ms,
            max_bytes,
            frame_rate,
            ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let action_id = session.allocate_action_id();
                // Resolve optional caps to the safe defaults (plan D5).
                let dur = max_duration_ms.unwrap_or(300_000);
                let bytes = max_bytes.unwrap_or(268_435_456);
                let fps = frame_rate.map(|f| f.clamp(1, 60) as u32).unwrap_or(10);
                match handle.block_on(host.start_recording(&sid, dur, bytes, fps)) {
                    Ok(()) => {
                        return Ok(build_recording_started_receipt(action_id, session_id_str))
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "recording_start_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }
        if let Action::WebStopRecording { .. } = action {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let action_id = session.allocate_action_id();
                match handle.block_on(host.stop_recording(&sid)) {
                    Ok(result) => {
                        return Ok(build_stop_recording_receipt(
                            action_id,
                            session_id_str,
                            result,
                        ))
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "recording_stop_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }

        // voice-call-io (task 04): web.inject_audio is a host/shim side-channel —
        // it does NOT run the WASM guest, navigate, or enter the replay hash chain
        // (the receipt carries a CONSTANT outcome_hash). The daemon resolves the
        // payload (blob-ref → CAS / inline base64) and size-bounds it here so the
        // shim stays CAS-free (PRD D12); a real WebRTC call is inherently non-
        // deterministic, so audio verbs hard-error on a determinism-enabled session
        // BEFORE touching the shim — a paused virtual clock would freeze the call
        // (PRD D5).
        if let Action::WebInjectAudio {
            blob_ref,
            audio_b64,
            await_playout,
            ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let action_id = session.allocate_action_id();

                // D5: audio requires no_determinism.
                if !session.no_determinism {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "determinism_enabled",
                        "web.inject_audio requires a no-determinism session (a paused \
                     virtual clock would freeze the live call); create the session \
                     with no-determinism enabled"
                            .to_string(),
                    ));
                }

                // Resolve + size-bound the payload daemon-side (A5, D12).
                let bytes = match resolve_inject_payload(
                    blob_ref.as_deref(),
                    audio_b64.as_deref(),
                    &self.core.content_store,
                ) {
                    Ok(b) => b,
                    Err((kind, message)) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            kind,
                            message,
                        ))
                    }
                };

                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let await_playout = await_playout.unwrap_or(false);
                match handle.block_on(host.inject_audio(&sid, bytes, await_playout)) {
                    Ok(outcome) => {
                        // D18 observability: the daemon logs enqueue/playout completion;
                        // it is deliberately NOT a receipt field (plan-council: no public
                        // schema creep) — the receipt carries only the constant hash.
                        tracing::info!(
                            session_id = %sid,
                            duration_ms = outcome.duration_ms,
                            awaited_playout = outcome.awaited_playout,
                            "audio.inject_enqueued"
                        );
                        return Ok(build_inject_audio_receipt(action_id, session_id_str));
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            classify_inject_error(&msg),
                            msg,
                        ));
                    }
                }
            })());
        }

        // voice-call-io (task 06): web.start_audio_capture / web.stop_audio_capture
        // are host/shim side-channels — no WASM guest, no navigate, no hash chain,
        // exactly like recording. The captured WAV is written to CAS host-side; the
        // stop receipt carries the observational `audio_after_hash` (which lives
        // OUTSIDE the manifest chain) plus a CONSTANT per-verb `outcome_hash`. Like
        // inject, capture hard-errors on a determinism-enabled session BEFORE
        // touching the shim — a paused virtual clock would freeze the live call
        // (PRD D5). The capture target is always the session's OWN live target
        // (resolved host-side via `shim_session_id_for`); the wire action carries no
        // caller-supplied `target_id`, so a cross-session target cannot be forged.
        if let Action::WebStartAudioCapture {
            max_duration_ms,
            max_bytes,
            ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let action_id = session.allocate_action_id();
                if !session.no_determinism {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "determinism_enabled",
                        "web.start_audio_capture requires a no-determinism session (a paused \
                     virtual clock would freeze the live call); create the session \
                     with no-determinism enabled"
                            .to_string(),
                    ));
                }
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                // Resolve optional caps to safe defaults; the shim re-clamps via
                // `Caps::sanitized`, so the public API can never disable the caps.
                let dur = max_duration_ms.unwrap_or(300_000);
                // Default byte cap sized NOT to truncate before the duration default:
                // 16 kHz mono i16 ≈ 32 KB/s, so 300 s ≈ 9.6 MiB — 16 MiB gives headroom
                // (still well under the 64 MiB host ceiling). Callers pass max_bytes for a
                // tighter bound. (#4: the previous 1 MiB default truncated at ~32 s.)
                let bytes = max_bytes.unwrap_or(16 * 1024 * 1024);
                match handle.block_on(host.start_audio_capture(&sid, dur, bytes)) {
                    Ok(()) => {
                        return Ok(build_audio_capture_started_receipt(
                            action_id,
                            session_id_str,
                        ))
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "audio_capture_start_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }
        if let Action::WebStopAudioCapture { .. } = action {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let action_id = session.allocate_action_id();
                if !session.no_determinism {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "determinism_enabled",
                        "web.stop_audio_capture requires a no-determinism session (a paused \
                     virtual clock would freeze the live call); create the session \
                     with no-determinism enabled"
                            .to_string(),
                    ));
                }
                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                match handle.block_on(host.stop_audio_capture(&sid)) {
                    Ok(result) => {
                        return Ok(build_stop_audio_capture_receipt(
                            action_id,
                            session_id_str,
                            result,
                        ))
                    }
                    Err(e) => {
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            "audio_capture_stop_failed",
                            e.to_string(),
                        ))
                    }
                }
            })());
        }

        // voice-call-io (task 09): web.say is a host/shim side-channel that layers on
        // inject_audio — the daemon synthesizes `text` → audio bytes via the
        // operator-configured TTS backend (LOOM_TTS_CMD / LOOM_TTS_URL, hardened in
        // tts_backend.rs) and feeds them through the SAME inject path, so `say`
        // inherits every inject bound (size cap, replay exclusion). Like inject, it
        // hard-errors on a determinism-enabled session BEFORE synthesizing — a paused
        // virtual clock would freeze the live call (PRD D5). `text` is NEVER logged
        // (it can be sensitive call audio — plan-council P-A3); only its length and
        // the failure class are observable.
        if let Action::WebSay {
            text,
            await_playout,
            ..
        } = action
        {
            return Some((|| -> Result<Receipt, HostAdapterError> {
                let action_id = session.allocate_action_id();

                if !session.no_determinism {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "determinism_enabled",
                        "web.say requires a no-determinism session (a paused virtual clock \
                     would freeze the live call); create the session with no-determinism \
                     enabled"
                            .to_string(),
                    ));
                }

                // Synthesize daemon-side (subprocess / SSRF-guarded HTTP). A typed TtsError
                // maps straight to the receipt error.kind; the Display message never echoes
                // `text` (P-A3).
                let text_len = text.len();
                let bytes = match handle.block_on(crate::tts_backend::synthesize(text)) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            session_id = %session_id_str,
                            kind = e.kind(),
                            text_len,
                            "audio.say_tts_failed"
                        );
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            e.kind(),
                            e.to_string(),
                        ));
                    }
                };

                // A14: synthesized bytes pass the SAME inject size bound as any payload.
                if bytes.len() > MAX_INJECT_BYTES {
                    return Ok(recording_error_receipt(
                        action_id,
                        session_id_str,
                        "payload_too_large",
                        format!(
                            "synthesized audio is {} bytes, over the {MAX_INJECT_BYTES}-byte cap",
                            bytes.len()
                        ),
                    ));
                }

                let host = Arc::clone(&self.host);
                let sid = session_id_str.to_string();
                let await_playout = await_playout.unwrap_or(false);
                match handle.block_on(host.inject_audio(&sid, bytes, await_playout)) {
                    Ok(outcome) => {
                        tracing::info!(
                            session_id = %sid,
                            text_len,
                            duration_ms = outcome.duration_ms,
                            awaited_playout = outcome.awaited_playout,
                            "audio.say_enqueued"
                        );
                        return Ok(build_say_receipt(action_id, session_id_str));
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        return Ok(recording_error_receipt(
                            action_id,
                            session_id_str,
                            classify_inject_error(&msg),
                            msg,
                        ));
                    }
                }
            })());
        }
        None
    }
}
