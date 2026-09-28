// ShimManager — screencast recording and the voice-call audio senders (capture,
// inject). Split out of `senders.rs` (behaviour-preserving).

use super::helpers::{map_shim_code, shim_error_class};
use super::process::send_and_await;
use super::shim_manager::ShimManager;
use super::types::{FailureClass, ShimId};
use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::navigate_outcome::{AudioCaptureOutcome, AudioInjectOutcome, ScreencastOutcome};
use loom_shared::shim_protocol::{ShimRequest, ShimResponse};
use std::time::Duration;

impl ShimManager {
    /// video-capture: start a screencast recording on the target
    /// (`ShimRequest::StartRecording`). Returns `Ok(())` on confirmation; a
    /// recording-already-active / startScreencast failure surfaces as a typed
    /// `LoomError`.
    pub async fn send_start_recording(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        max_duration_ms: u64,
        max_bytes: u64,
        frame_rate: u32,
    ) -> Result<(), LoomError> {
        self.check_breaker(&id)?;
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(&id, &config).await?;
        let request = ShimRequest::StartRecording {
            request_id: 0,
            session_id,
            target_id,
            max_duration_ms,
            max_bytes,
            frame_rate,
        };
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(config.recv_timeout_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { .. }) => {
                self.record_success(&id);
                Ok(())
            }
            Ok(ShimResponse::Error { code, detail, .. }) => {
                self.record_failure(&id, shim_error_class(&code));
                Err(LoomError::new(
                    map_shim_code(code),
                    format!("shim {}: {}", id.0, detail),
                ))
            }
            Ok(other) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: unexpected non-Ok response: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }

    /// video-capture: stop the active recording (`ShimRequest::StopRecording`)
    /// and return the decoded `ScreencastOutcome` (the host then writes the
    /// `webm_bytes` to CAS). A LONGER receive timeout is used because the shim
    /// runs the ffmpeg encode synchronously before responding.
    pub async fn send_stop_recording(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
    ) -> Result<ScreencastOutcome, LoomError> {
        self.check_breaker(&id)?;
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(&id, &config).await?;
        let request = ShimRequest::StopRecording {
            request_id: 0,
            session_id,
            target_id,
        };
        // Encoding can take seconds for a multi-minute recording; give the
        // recv a generous floor independent of the per-CDP-command budget.
        let recv_timeout = Duration::from_millis(config.recv_timeout_ms.max(120_000));
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            recv_timeout,
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: stop_recording response re-encode: {e}", id.0),
                    ));
                }
                loom_shared::shim_protocol::ciborium_from_slice::<ScreencastOutcome>(&bytes)
                    .map_err(|e| {
                        LoomError::new(
                            LoomErrorCode::ShimFailure,
                            format!("shim {}: screencast outcome decode: {e}", id.0),
                        )
                    })
            }
            Ok(ShimResponse::Error { code, detail, .. }) => {
                self.record_failure(&id, shim_error_class(&code));
                Err(LoomError::new(
                    map_shim_code(code),
                    format!("shim {}: {}", id.0, detail),
                ))
            }
            Ok(other) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: unexpected non-Ok response: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }

    /// voice-call-io (task 06): start capturing inbound audio on the session's
    /// active page target (`ShimRequest::StartAudioCapture`). Returns `Ok(())` on
    /// confirmation; audio-not-enabled / double-start surfaces as a typed error.
    pub async fn send_start_audio_capture(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        max_duration_ms: u64,
        max_bytes: u64,
    ) -> Result<(), LoomError> {
        self.check_breaker(&id)?;
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(&id, &config).await?;
        let request = ShimRequest::StartAudioCapture {
            request_id: 0,
            session_id,
            target_id,
            max_duration_ms,
            max_bytes,
        };
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(config.recv_timeout_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { .. }) => {
                self.record_success(&id);
                Ok(())
            }
            Ok(ShimResponse::Error { code, detail, .. }) => {
                self.record_failure(&id, shim_error_class(&code));
                Err(LoomError::new(
                    map_shim_code(code),
                    format!("shim {}: {}", id.0, detail),
                ))
            }
            Ok(other) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: unexpected non-Ok response: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }

    /// voice-call-io (task 06): stop the active capture (`ShimRequest::StopAudioCapture`)
    /// and return the decoded [`AudioCaptureOutcome`] (the host then writes the
    /// `wav_bytes` to CAS). A longer recv timeout covers the synchronous
    /// drain + resample + WAV-mux the shim runs before responding.
    pub async fn send_stop_audio_capture(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
    ) -> Result<AudioCaptureOutcome, LoomError> {
        self.check_breaker(&id)?;
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(&id, &config).await?;
        let request = ShimRequest::StopAudioCapture {
            request_id: 0,
            session_id,
            target_id,
        };
        // Drain+resample+mux runs synchronously before the response; give the recv
        // a generous floor independent of the per-CDP-command budget.
        let recv_timeout = Duration::from_millis(config.recv_timeout_ms.max(30_000));
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            recv_timeout,
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: stop_audio_capture response re-encode: {e}", id.0),
                    ));
                }
                loom_shared::shim_protocol::ciborium_from_slice::<AudioCaptureOutcome>(&bytes)
                    .map_err(|e| {
                        LoomError::new(
                            LoomErrorCode::ShimFailure,
                            format!("shim {}: audio capture outcome decode: {e}", id.0),
                        )
                    })
            }
            Ok(ShimResponse::Error { code, detail, .. }) => {
                self.record_failure(&id, shim_error_class(&code));
                Err(LoomError::new(
                    map_shim_code(code),
                    format!("shim {}: {}", id.0, detail),
                ))
            }
            Ok(other) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: unexpected non-Ok response: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }

    /// voice-call-io (task 04): inject daemon-resolved audio bytes into the
    /// session's synthetic microphone (`ShimRequest::InjectAudio`) and return the
    /// decoded [`AudioInjectOutcome`]. When `await_playout` is set the shim blocks
    /// until playout completes (bounded shim-side at 60 s), so the recv timeout is
    /// floored above that ceiling; otherwise the enqueue-only dispatch returns fast.
    pub async fn send_inject_audio(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        audio_bytes: Vec<u8>,
        await_playout: bool,
    ) -> Result<AudioInjectOutcome, LoomError> {
        self.check_breaker(&id)?;
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(&id, &config).await?;
        let request = ShimRequest::InjectAudio {
            request_id: 0,
            session_id,
            target_id,
            audio_bytes,
            await_playout,
        };
        // await_playout blocks the shim until the clip finishes (≤ 60 s shim
        // ceiling); floor the recv above that so the host doesn't time out first.
        let recv_floor = if await_playout { 90_000 } else { 10_000 };
        let recv_timeout = Duration::from_millis(config.recv_timeout_ms.max(recv_floor));
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            recv_timeout,
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: inject_audio response re-encode: {e}", id.0),
                    ));
                }
                loom_shared::shim_protocol::ciborium_from_slice::<AudioInjectOutcome>(&bytes)
                    .map_err(|e| {
                        LoomError::new(
                            LoomErrorCode::ShimFailure,
                            format!("shim {}: audio inject outcome decode: {e}", id.0),
                        )
                    })
            }
            Ok(ShimResponse::Error { code, detail, .. }) => {
                // Inject "errors" are predominantly USER-level typed outcomes
                // (no_microphone_request / audio_decode_failed / audio_not_enabled)
                // — they must NOT trip the circuit breaker, which exists to detect
                // shim transport/crash health. Genuine transport failures surface on
                // the `Err(e)` arm below (which does record_failure). `detail` carries
                // the typed KIND verbatim so the daemon can derive the receipt kind.
                tracing::warn!(shim = %id.0, detail = %detail, "web.inject_audio failed");
                Err(LoomError::new(
                    map_shim_code(code),
                    format!("shim {}: {}", id.0, detail),
                ))
            }
            Ok(other) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: unexpected non-Ok response: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }
}
