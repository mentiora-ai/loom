// ShimManager — typed per-verb senders.
//
// One `impl ShimManager` block carrying the typed verb senders that wrap a
// `ShimRequest` round-trip and decode the response into a domain outcome:
// `send_navigate`, `send_network_log`, `send_start_recording`,
// `send_stop_recording`, `send_wait_for`, `send_evaluate`, and
// `send_set_input_files`. Split out of `shim_manager.rs` (behavior-preserving).
//
// The four formerly-`#[allow(clippy::too_many_arguments)]` senders now take a
// dedicated params struct (see `super::types`); the generic `send` and the
// lifecycle / breaker methods stay in `shim_manager.rs`.

use super::helpers::{cbor_get, cbor_u64, map_shim_code, parse_evaluate_payload, shim_error_class};
use super::input_dispatch::{
    dispatch_frame_step, fill_prepare_message, insert_text_event, keystroke_events_for_text,
    mouse_event, parse_fill_prepare, press_key_events, release_fill_objects_message,
    resolve_node_message, FillPrep, FillPrepFailure, FrameAck, FrameStep,
};
use super::process::{send_and_await, send_and_await_dispatch, DispatchAck};
use super::shim_manager::ShimManager;
use super::types::{
    EvaluateOutcome, FailureClass, InputDispatchOutcome, SendEvaluateParams, SendNavigateParams,
    SendPressKeyParams, SendSetInputFilesParams, SendWaitForParams, SetInputFilesOutcome, ShimId,
    WaitResolveOutcome,
};
use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::locator::{parse_locator, Segment};
use loom_shared::navigate_outcome::{
    AudioCaptureOutcome, AudioInjectOutcome, NavigateOutcome, NetworkLogOutcome, ScreencastOutcome,
};
use loom_shared::shim_protocol::{CdpMessage, ShimErrorCode, ShimRequest, ShimResponse};
use std::time::Duration;

/// Result of [`ShimManager::dispatch_input_events`]: all input frames were acked
/// (`Acked`), or the committing frame was written but its ack was lost to a
/// likely cross-origin renderer swap (`Dispatched`). Maps to the trusted-input
/// verb's `InputDispatchOutcome`.
enum InputAck {
    Acked,
    Dispatched,
}

impl InputAck {
    fn into_outcome(self) -> InputDispatchOutcome {
        match self {
            InputAck::Acked => InputDispatchOutcome::Ok,
            InputAck::Dispatched => InputDispatchOutcome::DispatchedAckPending,
        }
    }
}

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
/// Numbers each `web.type` fill's CDP object group, so releasing one fill's
/// remote objects can never free another in-flight fill's.
static FILL_OBJECT_GROUP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// Ceiling on waiting for the best-effort release of a fill's object group.
const FILL_RELEASE_BUDGET_MS: u64 = 1_000;

impl ShimManager {
    /// Send a typed PageNavigate request and decode the response as
    /// `NavigateOutcome`. Uses `ShimRequest::PageNavigate` (not CdpSend).
    /// See [`SendNavigateParams`] for the per-call fields (`budget_ms`
    /// overrides the recv timeout when larger than the default; `action_id`
    /// is the WASM-guest-computed action hash threaded for host-side receipt
    /// correlation / observability — the shim does not see it; `seed` /
    /// `epoch_ms` ride the wire to the shim's determinism JS template;
    /// `blocklist_enabled` toggles the shim's `Fetch.enable` interception
    /// path).
    pub async fn send_navigate(
        &self,
        params: SendNavigateParams,
    ) -> Result<NavigateOutcome, LoomError> {
        let SendNavigateParams {
            id,
            action_id,
            session_id,
            target_id,
            url,
            budget_ms,
            seed,
            epoch_ms,
            blocklist_enabled,
            until,
            determinism_enabled,
            audio_enabled,
        } = params;
        // action_id is reserved for receipt correlation (Q5 plumbing); not
        // sent to the shim — shim deals only with target_id + CDP frames.
        let _action_id = action_id;
        self.check_breaker(&id)?;

        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;

        let process = self.get_or_spawn(&id, &config).await?;

        let request = ShimRequest::PageNavigate {
            request_id: 0, // overwritten by send_and_await
            session_id,
            target_id,
            url,
            seed,
            epoch_ms,
            // Per-session toggle from `--no-blocklist`. Default `true`
            // (enforce); `false` when the operator opted out.
            blocklist_enabled,
            // settle-capture: readiness mode gating the capture.
            until,
            // settle-capture (4b): per-session determinism toggle.
            determinism_enabled,
            // voice-call-io: per-session audio opt-in (installs the mic bootstrap
            // on the lazy-spawned target).
            audio_enabled,
        };

        // Use the larger of budget_ms and recv_timeout_ms so callers can
        // extend the timeout for slow pages without touching the config.
        let recv_ms = budget_ms.max(config.recv_timeout_ms);

        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                // Re-encode the ciborium Value to bytes so we can use
                // ciborium_from_slice → NavigateOutcome deserialization.
                // Field names in ActionResult::Navigated match NavigateOutcome
                // exactly; unknown fields (kind, target_id, frame_id, loader_id)
                // are silently ignored by serde.
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: navigate response re-encode: {e}", id.0),
                    ));
                }
                loom_shared::shim_protocol::ciborium_from_slice::<NavigateOutcome>(&bytes).map_err(
                    |e| {
                        LoomError::new(
                            LoomErrorCode::ShimFailure,
                            format!("shim {}: navigate outcome decode: {e}", id.0),
                        )
                    },
                )
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

    /// Read the shim's full-capture network-entries accumulator (everything
    /// observed since the last navigate). Observation-only — no CDP round-trip,
    /// no navigate. Backs the `loom.web.network_log` tool.
    pub async fn send_network_log(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
    ) -> Result<NetworkLogOutcome, LoomError> {
        self.check_breaker(&id)?;
        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(&id, &config).await?;
        let request = ShimRequest::GetNetworkLog {
            request_id: 0, // overwritten by send_and_await
            session_id,
            target_id,
        };
        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(config.recv_timeout_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: network_log response re-encode: {e}", id.0),
                    ));
                }
                loom_shared::shim_protocol::ciborium_from_slice::<NetworkLogOutcome>(&bytes)
                    .map_err(|e| {
                        LoomError::new(
                            LoomErrorCode::ShimFailure,
                            format!("shim {}: network_log outcome decode: {e}", id.0),
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

    /// settle-capture slice 2: run a standalone readiness wait on the session's
    /// current target via `ShimRequest::WaitFor`, parsing the response into a
    /// typed `WaitOutcome`. Mirrors `send_evaluate`: an idempotent SpawnTarget
    /// first so the wait runs against the determinism-injected target (not the
    /// bootstrap about:blank), then the typed wait request. See
    /// [`SendWaitForParams`] for the per-call fields.
    pub async fn send_wait_for(
        &self,
        params: SendWaitForParams,
    ) -> Result<loom_shared::navigate_outcome::WaitOutcome, LoomError> {
        let SendWaitForParams {
            id,
            action_id,
            session_id,
            target_id,
            until,
            budget_ms,
            seed,
            epoch_ms,
            determinism_enabled,
            audio_enabled,
        } = params;
        // action_id reserved for receipt correlation (Q5 plumbing).
        let _action_id = action_id;

        self.check_breaker(&id)?;

        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;

        let process = self.get_or_spawn(&id, &config).await?;

        // Idempotent lazy-spawn (same rationale as send_evaluate): ensures the
        // wait runs against the seeded target, never the about:blank bootstrap.
        let spawn_request = ShimRequest::SpawnTarget {
            request_id: 0,
            session_id,
            profile: "default".to_string(),
            seed,
            epoch_ms,
            // settle-capture (4b): per-session determinism toggle.
            determinism_enabled,
            // voice-call-io: per-session audio opt-in (installs the mic bootstrap).
            audio_enabled,
        };
        let _ = send_and_await(
            &process,
            spawn_request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(config.recv_timeout_ms),
        )
        .await;

        let request = ShimRequest::WaitFor {
            request_id: 0,
            session_id,
            target_id,
            until,
            // Thread the daemon's per-call settle budget to the shim so its whole
            // multi-phase wait (arm + reattach + resume) shares ONE bounded
            // deadline. Without this the shim used its env base per phase and a
            // cross-origin process swap overran the RPC deadline (v0.15.0 bug).
            budget_ms: Some(budget_ms),
        };

        // The transport recv MUST stay a superset of the shim's own budget — the
        // shim returns a typed verdict at `budget_ms`; the host must not cut the
        // connection first.
        let recv_ms = budget_ms.max(config.recv_timeout_ms);

        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                // Re-encode the ciborium Value, then decode as WaitOutcome.
                // Field names in ActionResult::Waited match WaitOutcome; the
                // `kind` tag is ignored by serde.
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(&payload, &mut bytes) {
                    return Err(LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: wait_for response re-encode: {e}", id.0),
                    ));
                }
                loom_shared::shim_protocol::ciborium_from_slice::<
                    loom_shared::navigate_outcome::WaitOutcome,
                >(&bytes)
                .map_err(|e| {
                    LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: wait_for outcome decode: {e}", id.0),
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

    /// Send `Runtime.evaluate` against `id`'s target via CdpSend and parse
    /// the response into a typed `EvaluateOutcome`. See [`SendEvaluateParams`]
    /// for the per-call fields (`action_id` is the WASM-guest-computed action
    /// hash threaded for receipt correlation; not sent to the shim).
    ///
    /// CDP `Runtime.evaluate` shape:
    ///   request:  {expression, returnByValue:true, awaitPromise:true}
    ///   response: { result: {type, value?}, exceptionDetails?: {...} }
    /// On exceptionDetails the host wraps as HostError::ShimFailure with
    /// `{kind:"js_throw", exception:..., line:..., column:...}` JSON.
    pub async fn send_evaluate(
        &self,
        params: SendEvaluateParams,
    ) -> Result<EvaluateOutcome, LoomError> {
        let SendEvaluateParams {
            id,
            action_id,
            session_id,
            target_id,
            expression,
            budget_ms,
            seed,
            epoch_ms,
            determinism_enabled,
            audio_enabled,
        } = params;
        // action_id reserved for receipt correlation (Q5 plumbing).
        let _action_id = action_id;

        self.check_breaker(&id)?;

        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;

        let process = self.get_or_spawn(&id, &config).await?;

        // Lazy-spawn the determinism-injected target before evaluating.
        // The shim's `CdpSend` handler does NOT do this on its own (only
        // `PageNavigate` does), so an evaluate-only flow would otherwise
        // route to the bootstrap about:blank context where Date.now /
        // Math.random still leak real wall-clock + unseeded values.
        // SpawnTarget is idempotent at the TargetManager level
        // so navigate-then-evaluate paths pay no extra cost.
        let spawn_request = ShimRequest::SpawnTarget {
            request_id: 0,
            session_id,
            profile: "default".to_string(),
            seed,
            epoch_ms,
            // settle-capture (4b): per-session determinism toggle.
            determinism_enabled,
            // voice-call-io: per-session audio opt-in (installs the mic bootstrap).
            audio_enabled,
        };
        // Best-effort: if SpawnTarget fails (e.g. unknown shim error),
        // fall through to the eval anyway and surface the eval's own
        // error path. The most common failure here is "target already
        // exists" which the dispatcher treats as Ok.
        let _ = send_and_await(
            &process,
            spawn_request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(config.recv_timeout_ms),
        )
        .await;

        // Build CDP Runtime.evaluate params as a CBOR map. `returnByValue`
        // gives us the value back as CBOR (vs. an opaque object handle);
        // `awaitPromise` resolves promises within the budget window.
        let params = ciborium::value::Value::Map(vec![
            (
                ciborium::value::Value::Text("expression".into()),
                ciborium::value::Value::Text(expression),
            ),
            (
                ciborium::value::Value::Text("returnByValue".into()),
                ciborium::value::Value::Bool(true),
            ),
            (
                ciborium::value::Value::Text("awaitPromise".into()),
                ciborium::value::Value::Bool(true),
            ),
        ]);

        let request = ShimRequest::CdpSend {
            request_id: 0,
            session_id,
            target_id,
            message: CdpMessage {
                method: "Runtime.evaluate".into(),
                params,
            },
        };

        let recv_ms = budget_ms.max(config.recv_timeout_ms);

        match send_and_await(
            &process,
            request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => {
                self.record_success(&id);
                parse_evaluate_payload(&payload).map_err(|e| {
                    LoomError::new(
                        LoomErrorCode::ShimFailure,
                        format!("shim {}: evaluate response parse: {e}", id.0),
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

    /// Resolve a CSS selector to a file input and set its files via CDP
    /// `DOM.setFileInputFiles`. Issues the sequence
    /// `DOM.getDocument` → `DOM.querySelector` → `DOM.setFileInputFiles`
    /// against the session's target. Paths are already validated +
    /// canonicalized daemon-side (upload_guard) before reaching here. See
    /// [`SendSetInputFilesParams`] for the per-call fields.
    ///
    /// Outcomes:
    ///   - `Ok(SetInputFilesOutcome::Ok { file_count })` on success.
    ///   - `Ok(SelectorNotFound)` when querySelector returns nodeId == 0.
    ///   - `Ok(NotAFileInput)` when setFileInputFiles errors on a resolved node.
    ///   - `Err(LoomError)` for transport / breaker / protocol failures.
    pub async fn send_set_input_files(
        &self,
        params: SendSetInputFilesParams,
    ) -> Result<SetInputFilesOutcome, LoomError> {
        use ciborium::value::{Integer, Value};
        let SendSetInputFilesParams {
            id,
            action_id,
            session_id,
            target_id,
            selector,
            files,
            budget_ms,
            seed,
            epoch_ms,
            determinism_enabled,
            audio_enabled,
        } = params;
        let _action_id = action_id;

        self.check_breaker(&id)?;

        let config = self.configs.get(&id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;

        let process = self.get_or_spawn(&id, &config).await?;
        let recv_ms = budget_ms.max(config.recv_timeout_ms);

        // Lazy-spawn the session target (idempotent), same as send_evaluate —
        // so a set_input_files before an explicit SpawnTarget still resolves
        // against a real target rather than the bootstrap context.
        let spawn_request = ShimRequest::SpawnTarget {
            request_id: 0,
            session_id,
            profile: "default".to_string(),
            seed,
            epoch_ms,
            // settle-capture (4b): per-session determinism toggle.
            determinism_enabled,
            // voice-call-io: per-session audio opt-in (installs the mic bootstrap).
            audio_enabled,
        };
        let _ = send_and_await(
            &process,
            spawn_request,
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(config.recv_timeout_ms),
        )
        .await;

        // One raw CdpSend round-trip → raw CDP result Value (or Err on a
        // shim-level error envelope). `step_err_is_app` lets the caller treat
        // a CDP error at a specific step as an application outcome.
        let cdp = |method: &'static str, params: Value| {
            let process = process.clone();
            let send_to = Duration::from_millis(config.send_timeout_ms);
            let recv_to = Duration::from_millis(recv_ms);
            async move {
                let request = ShimRequest::CdpSend {
                    request_id: 0,
                    session_id,
                    target_id,
                    message: CdpMessage {
                        method: method.into(),
                        params,
                    },
                };
                send_and_await(&process, request, send_to, recv_to).await
            }
        };

        // Resolve the (possibly grammar-prefixed: `css=` / `text=` / `role=` /
        // `frame=`) locator to a node id via the SAME frame-aware resolver
        // web.click / web.type use — descending same-process iframes for
        // cross-origin reach. The prior code passed the selector STRAIGHT to
        // `DOM.querySelector`, so any locator-grammar form (the documented
        // selector syntax) or a non-match resolved to nothing and surfaced as a
        // `surface_trap` (`ShimFailure → SurfaceTrap` at the rpc layer) rather
        // than attaching the file or returning a typed `selector_not_found`.
        // #223 added the grammar to click/type but missed this verb (it predates
        // it). `Ok(None)` here = genuine no-match (incl. a `text=`/`role=` miss or
        // an out-of-process frame) → typed application outcome, not a trap.
        let node_id = match self
            .resolve_locator_node(&id, session_id, target_id, &selector, recv_ms)
            .await
        {
            Ok(Some(n)) => n,
            Ok(None) => {
                self.record_success(&id);
                return Ok(SetInputFilesOutcome::SelectorNotFound);
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                return Err(e);
            }
        };

        // Step 3: DOM.setFileInputFiles(nodeId, files). A CDP error on a
        // RESOLVED node means it isn't a file input (or it rejected the files).
        let file_count = files.len() as u32;
        let files_val = Value::Array(files.into_iter().map(Value::Text).collect());
        let set_resp = cdp(
            "DOM.setFileInputFiles",
            Value::Map(vec![
                (
                    Value::Text("nodeId".into()),
                    Value::Integer(Integer::from(node_id)),
                ),
                (Value::Text("files".into()), files_val),
            ]),
        )
        .await;
        match set_resp {
            Ok(ShimResponse::Ok { .. }) => {
                self.record_success(&id);
                Ok(SetInputFilesOutcome::Ok { file_count })
            }
            Ok(ShimResponse::Error { .. }) => {
                // Node resolved but setFileInputFiles rejected it → not a file input.
                // This is an application outcome, not a shim breaker failure.
                self.record_success(&id);
                Ok(SetInputFilesOutcome::NotAFileInput)
            }
            Ok(other) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(LoomError::new(
                    LoomErrorCode::ShimFailure,
                    format!("shim {}: setFileInputFiles unexpected: {other:?}", id.0),
                ))
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Transport);
                Err(e)
            }
        }
    }

    // ─── Trusted CDP input dispatch (cdp-trusted-input) ──────────────────────
    //
    // `web.type mode:keystrokes`, `web.press_key`, and the always-trusted
    // `web.click` drive REAL (`isTrusted:true`) browser input via CDP `Input.*`.
    // Each orchestrates a multi-step CDP sequence host-side from raw `CdpSend`
    // round-trips — the same shape as `send_set_input_files`, so the shim and
    // wire protocol are untouched.

    /// One CDP round-trip. `Ok(Ok(payload))` = CDP success; `Ok(Err((code,
    /// detail)))` = the shim reported a CDP-protocol error (an APPLICATION
    /// outcome the caller interprets, e.g. `getBoxModel` on a hidden node);
    /// `Err(LoomError)` = transport failure. Breaker bookkeeping is done by the
    /// caller at its decision points.
    async fn cdp_send_one(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        message: CdpMessage,
        budget_ms: u64,
    ) -> Result<Result<ciborium::value::Value, (ShimErrorCode, String)>, LoomError> {
        let config = self.configs.get(id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(id, &config).await?;
        let recv_ms = budget_ms.max(config.recv_timeout_ms);
        match send_and_await(
            &process,
            ShimRequest::CdpSend {
                request_id: 0,
                session_id,
                target_id,
                message,
            },
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => Ok(Ok(payload)),
            Ok(ShimResponse::Error { code, detail, .. }) => Ok(Err((code, detail))),
            Ok(other) => Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {}: unexpected CDP response: {other:?}", id.0),
            )),
            Err(e) => Err(e),
        }
    }

    /// Resolve `selector` to a node and focus it. `Ok(Some(nodeId))` focused;
    /// `Ok(None)` selector matched nothing; `Err` on transport failure. Focus
    /// itself is best-effort (a non-focusable node still receives dispatched key
    /// events). The nodeId is only valid until the next `DOM.getDocument`, so a
    /// caller acts on it straight away.
    async fn resolve_and_focus(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        selector: &str,
        budget_ms: u64,
    ) -> Result<Option<u64>, LoomError> {
        use ciborium::value::{Integer, Value};
        // Frame-aware resolution (descends same-process cross-origin iframes);
        // for a bare/plain CSS selector this is the same getDocument →
        // querySelector path as before.
        let node = match self
            .resolve_locator_node(id, session_id, target_id, selector, budget_ms)
            .await?
        {
            Some(n) => n,
            None => return Ok(None),
        };
        // Best-effort focus — ignore a CDP error (non-focusable element).
        // `Input.insertText`/`dispatchKeyEvent` then target the focused element,
        // which is correct even when it lives inside a cross-origin frame.
        let _ = self
            .cdp_send_one(
                id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.focus".into(),
                    params: Value::Map(vec![(
                        Value::Text("nodeId".into()),
                        Value::Integer(Integer::from(node)),
                    )]),
                },
                budget_ms,
            )
            .await?;
        Ok(Some(node))
    }

    /// One trusted-INPUT CDP round-trip whose ack may be lost to a cross-origin
    /// renderer swap. Unlike [`cdp_send_one`] (whose `budget_ms.max(recv_timeout_ms)`
    /// floor is unchanged, so selector resolution and `web.wait`'s `budget=0`
    /// probes keep their ~30s bound), the recv here is BOUNDED by `budget_ms`, and
    /// a recv timeout is reported as `Ok(None)` (the frame was written, its ack
    /// never came) instead of a full-`recv_timeout` dead-wait. The daemon threads a
    /// real (non-zero) budget to every trusted-input verb; the `budget_ms == 0`
    /// fallback to the config `recv_timeout_ms` floor is a safety for any DIRECT
    /// caller that passes `0` (e.g. an in-crate test), so `0` never means a
    /// zero-length recv. `Ok(Some(Ok(v)))` = CDP success; `Ok(Some(Err(..)))` = CDP
    /// app error; `Ok(None)` = ack timed out within budget; `Err` = the frame never
    /// left the host.
    async fn cdp_send_dispatch(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        message: CdpMessage,
        budget_ms: u64,
    ) -> Result<Option<Result<ciborium::value::Value, (ShimErrorCode, String)>>, LoomError> {
        let config = self.configs.get(id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(id, &config).await?;
        // A normal `Input.*` ack returns in tens of ms, so bounding the recv at
        // the caller's budget only ever bites when the ack is genuinely lost (a
        // process swap). `budget_ms == 0` (no caller deadline) falls back to the
        // config floor so verbs that pass `0` are unaffected.
        let recv_ms = if budget_ms > 0 {
            budget_ms
        } else {
            config.recv_timeout_ms
        };
        match send_and_await_dispatch(
            &process,
            ShimRequest::CdpSend {
                request_id: 0,
                session_id,
                target_id,
                message,
            },
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await?
        {
            DispatchAck::Response(ShimResponse::Ok { payload, .. }) => Ok(Some(Ok(payload))),
            DispatchAck::Response(ShimResponse::Error { code, detail, .. }) => {
                Ok(Some(Err((code, detail))))
            }
            DispatchAck::Response(other) => Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {}: unexpected CDP response: {other:?}", id.0),
            )),
            DispatchAck::RecvTimeout => Ok(None),
        }
    }

    /// Dispatch a prebuilt sequence of `Input.*` frames. The per-frame ack decision
    /// is the pure [`dispatch_frame_step`] classifier (exhaustively unit-tested):
    /// a CDP app error aborts (`ReturnError`); a recv timeout on the frame at or
    /// after `commit_from_index` — the COMMITTING input (`mousePressed` for a click;
    /// index 0 for a key/text frame) — returns [`InputAck::Dispatched`] (the input
    /// was written but its ack was lost, the cross-origin process-swap case).
    ///
    /// A recv timeout BEFORE the committing frame (e.g. `mouseMoved`) no longer
    /// aborts eagerly: a cross-origin swap can open at/before the move and swallow
    /// that ack too, so we KEEP DISPATCHING the committing frames (the click is
    /// genuinely written) rather than mislabel a performed-but-swapped click a
    /// transport `shim_timeout`. If the committing frames instead ACK, the session
    /// is alive and the click committed cleanly, so a dropped `mouseMoved` ack ALONE
    /// does not degrade the outcome → `Acked`.
    ///
    /// **Wall-clock bounding.** Each frame is bounded by the per-frame `budget_ms`
    /// (unchanged from v0.15.2 — every caller, incl. the keystroke / fill / press-key
    /// paths, keeps identical timeout semantics). Tolerating the pre-commit frame
    /// adds AT MOST one extra timed-out frame (the `mouseMoved`) before the committing
    /// frame, so a swapped click's dispatch is bounded by ~2× `budget_ms` in the worst
    /// case — still bounded, never a dead-wait, and it fits inside the daemon's
    /// elapsed-aware settle budget under the per-call deadline. `budget_ms == 0` (a
    /// direct in-crate caller with no deadline) keeps the `config.recv_timeout_ms`
    /// fallback in `cdp_send_dispatch`.
    async fn dispatch_input_events(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        events: Vec<CdpMessage>,
        budget_ms: u64,
        commit_from_index: usize,
    ) -> Result<InputAck, LoomError> {
        debug_assert!(
            commit_from_index == 0 || commit_from_index < events.len(),
            "dispatch_input_events: commit_from_index {commit_from_index} out of range for {} frames \
             (a committing frame must be reachable, else a swapped click can't be reported dispatched)",
            events.len()
        );
        for (i, ev) in events.into_iter().enumerate() {
            let (ack, app_err) = match self
                .cdp_send_dispatch(id, session_id, target_id, ev, budget_ms)
                .await?
            {
                Some(Ok(_)) => (FrameAck::Acked, None),
                Some(Err((code, detail))) => (FrameAck::AppError, Some((code, detail))),
                None => (FrameAck::AckLost, None),
            };
            match dispatch_frame_step(i, commit_from_index, ack) {
                FrameStep::Advance => {
                    // The only non-clean advance is a tolerated pre-commit ack loss;
                    // log it (off-chain) so on-call can correlate a later swap.
                    if ack == FrameAck::AckLost {
                        tracing::debug!(
                            shim = %id.0,
                            target_id,
                            session_id,
                            frame_index = i,
                            commit_from_index,
                            "trusted-input pre-commit ack lost — continuing to the committing frames"
                        );
                    }
                }
                // The committing input was written but its ack was lost — the
                // cross-origin process-swap case. Off-chain debug signal so on-call
                // can correlate a `dispatched` click whose destination later fails to
                // settle (the primary production signal is the receipt's
                // `DispatchedAckPending` outcome + `settle_outcome`; the loud
                // `shim_timeout` this replaces is gone).
                FrameStep::ReturnDispatched => {
                    tracing::debug!(
                        shim = %id.0,
                        target_id,
                        session_id,
                        frame_index = i,
                        commit_from_index,
                        "trusted-input committing-frame ack lost (likely cross-origin \
                         renderer swap) — reporting dispatched"
                    );
                    return Ok(InputAck::Dispatched);
                }
                FrameStep::ReturnError => {
                    let (code, detail) =
                        app_err.expect("AppError frame ack carries the CDP error detail");
                    return Err(LoomError::new(
                        map_shim_code(code),
                        format!("shim {}: input dispatch: {detail}", id.0),
                    ));
                }
            }
        }
        // Every committing frame ACKed (a committing-frame timeout early-returns
        // `Dispatched`). The click committed and its commit ack arrived, so this is
        // a clean success even if an earlier `mouseMoved` ack was dropped.
        Ok(InputAck::Acked)
    }

    /// `web.type mode:keystrokes` — focus `selector`, then send a real per-char
    /// `Input.dispatchKeyEvent` (keyDown+text → keyUp) sequence.
    pub async fn send_type_keystrokes(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        text: String,
        budget_ms: u64,
    ) -> Result<InputDispatchOutcome, LoomError> {
        self.check_breaker(&id)?;
        if self
            .resolve_and_focus(&id, session_id, target_id, &selector, budget_ms)
            .await
            .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
            .is_none()
        {
            self.record_success(&id);
            return Ok(InputDispatchOutcome::SelectorNotFound);
        }
        match self
            .dispatch_input_events(
                &id,
                session_id,
                target_id,
                keystroke_events_for_text(&text),
                budget_ms,
                0,
            )
            .await
        {
            Ok(ack) => {
                self.record_success(&id);
                Ok(ack.into_outcome())
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Application);
                Err(e)
            }
        }
    }

    /// `web.type` DEFAULT (`mode:"fill"`) — Playwright `fill()` semantics, on the
    /// node `selector` RESOLVED to (never a re-query of the raw locator, which
    /// `document.querySelector` cannot parse for `role=`/`text=`/`css=`/`frame=`):
    /// focus it, then [`prepare_fill_target`](Self::prepare_fill_target) either
    /// sets a date/time-family input by value (Chromium ignores `Input.insertText`
    /// on those) or selects the existing content, which the one GENUINE
    /// (`isTrusted:true`) `Input.insertText` then replaces — so React /
    /// react-hook-form `onChange` fires and the value is treated as user-entered.
    /// A disabled/readonly target or a value the input rejects is a typed outcome,
    /// never a success with the field unchanged.
    pub async fn send_type_fill(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        text: String,
        budget_ms: u64,
    ) -> Result<InputDispatchOutcome, LoomError> {
        self.check_breaker(&id)?;
        let Some(node_id) = self
            .resolve_and_focus(&id, session_id, target_id, &selector, budget_ms)
            .await
            .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
        else {
            self.record_success(&id);
            return Ok(InputDispatchOutcome::SelectorNotFound);
        };
        let outcome = match self
            .prepare_fill_target(&id, session_id, target_id, node_id, &text, budget_ms)
            .await
        {
            Ok(FillPrep::ValueSet) => Ok(InputDispatchOutcome::Ok),
            Ok(FillPrep::Malformed(input_type)) => {
                Ok(InputDispatchOutcome::MalformedValue(input_type))
            }
            Ok(FillPrep::NotEditable) => Ok(InputDispatchOutcome::NotEditable),
            Ok(FillPrep::Insert) => self
                .dispatch_input_events(
                    &id,
                    session_id,
                    target_id,
                    vec![insert_text_event(&text)],
                    budget_ms,
                    0,
                )
                .await
                .map(InputAck::into_outcome),
            Err(e) => Err(e),
        };
        match &outcome {
            Ok(_) => self.record_success(&id),
            Err(_) => self.record_failure(&id, FailureClass::Application),
        }
        outcome
    }

    /// Run [`fill_prepare_fn`] on the resolved node: `DOM.resolveNode` →
    /// `Runtime.callFunctionOn` (the typed text travels as its argument) →
    /// `Runtime.releaseObjectGroup`. Every round-trip is bounded by the action's
    /// budget (`cdp_send_dispatch`). A lost ack before the verdict arrives is an
    /// error, not a dispatched input: the insert path has not typed anything yet
    /// and a set-value input may or may not have taken the value. The release is
    /// best-effort — its failure never changes a fill that already committed.
    async fn prepare_fill_target(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        node_id: u64,
        text: &str,
        budget_ms: u64,
    ) -> Result<FillPrep, LoomError> {
        let group = format!(
            "loom-fill-{}",
            FILL_OBJECT_GROUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let fill_error = |failure: FillPrepFailure| {
            let code = match failure {
                FillPrepFailure::Unacknowledged => LoomErrorCode::ShimTimeout,
                _ => LoomErrorCode::ShimFailure,
            };
            LoomError::new(code, format!("shim {}: {}", id.0, failure.message()))
        };
        let cdp_error = |step: &str, code: ShimErrorCode, detail: String| {
            LoomError::new(
                map_shim_code(code),
                format!("shim {}: {step}: {detail}", id.0),
            )
        };
        let resolved = self
            .cdp_send_dispatch(
                id,
                session_id,
                target_id,
                resolve_node_message(node_id, &group),
                budget_ms,
            )
            .await?
            .ok_or_else(|| fill_error(FillPrepFailure::Unacknowledged))?
            .map_err(|(code, detail)| cdp_error("DOM.resolveNode", code, detail))?;
        let object_id = match cbor_get(&resolved, "object").and_then(|o| cbor_get(o, "objectId")) {
            Some(ciborium::value::Value::Text(object_id)) => object_id.clone(),
            _ => return Err(fill_error(FillPrepFailure::NoObjectId)),
        };
        let verdict = self
            .cdp_send_dispatch(
                id,
                session_id,
                target_id,
                fill_prepare_message(&object_id, text),
                budget_ms,
            )
            .await;
        let _ = self
            .cdp_send_dispatch(
                id,
                session_id,
                target_id,
                release_fill_objects_message(&group),
                // `0` means "no deadline" (→ the 30 s floor); the release never waits that long.
                if budget_ms == 0 {
                    FILL_RELEASE_BUDGET_MS
                } else {
                    budget_ms.min(FILL_RELEASE_BUDGET_MS)
                },
            )
            .await;

        let payload = verdict?
            .ok_or_else(|| fill_error(FillPrepFailure::Unacknowledged))?
            .map_err(|(code, detail)| cdp_error("Runtime.callFunctionOn", code, detail))?;
        parse_fill_prepare(&payload).map_err(|failure| {
            // Only THAT something page-side went wrong is logged — never the page's
            // own text, which can echo the typed value.
            tracing::debug!(shim = %id.0, ?failure, "web.type fill prepare failed");
            fill_error(failure)
        })
    }

    /// `web.press_key` — optionally focus `selector`, then dispatch a named key
    /// (+ modifier combo) as real `Input.dispatchKeyEvent` frames. An unknown
    /// key / modifier is the typed `UnknownKey` outcome (not a transport error).
    pub async fn send_press_key(
        &self,
        params: SendPressKeyParams,
    ) -> Result<InputDispatchOutcome, LoomError> {
        let SendPressKeyParams {
            id,
            session_id,
            target_id,
            key,
            selector,
            modifiers,
            budget_ms,
        } = params;
        self.check_breaker(&id)?;
        if let Some(sel) = &selector {
            if self
                .resolve_and_focus(&id, session_id, target_id, sel, budget_ms)
                .await
                .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
                .is_none()
            {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::SelectorNotFound);
            }
        }
        let events = match press_key_events(&key, &modifiers) {
            Some(e) => e,
            None => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::UnknownKey);
            }
        };
        match self
            .dispatch_input_events(&id, session_id, target_id, events, budget_ms, 0)
            .await
        {
            Ok(ack) => {
                self.record_success(&id);
                Ok(ack.into_outcome())
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Application);
                Err(e)
            }
        }
    }

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
        let qs = self
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
            .map_err(|(code, detail)| {
                LoomError::new(map_shim_code(code), format!("shim {}: {detail}", id.0))
            })?;
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

        let doc = self
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
            .map_err(|(code, detail)| {
                LoomError::new(map_shim_code(code), format!("shim {}: {detail}", id.0))
            })?;
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
                    let described = self
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
                        .map_err(|(code, detail)| {
                            LoomError::new(map_shim_code(code), format!("shim {}: {detail}", id.0))
                        })?;
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
        let resp = self
            .cdp_send_one(id, session_id, target_id, eval(js), budget_ms)
            .await?
            .map_err(|(code, detail)| {
                LoomError::new(map_shim_code(code), format!("shim {}: {detail}", id.0))
            })?;
        let found = cbor_get(&resp, "result")
            .and_then(|r| cbor_get(r, "value"))
            .map(|v| matches!(v, Value::Bool(true)))
            .unwrap_or(false);
        if !found {
            return Ok(None);
        }
        let doc = self
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
            .map_err(|(code, detail)| {
                LoomError::new(map_shim_code(code), format!("shim {}: {detail}", id.0))
            })?;
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

    /// `Input.dispatchMouseEvent` mouseMoved→mousePressed→mouseReleased. No
    /// `el.click()` fallback. `SelectorNotFound` / `NotHittable` are typed
    /// outcomes; transport failures surface as `Err`.
    pub async fn send_trusted_click(
        &self,
        id: ShimId,
        session_id: u64,
        target_id: u64,
        selector: String,
        budget_ms: u64,
    ) -> Result<InputDispatchOutcome, LoomError> {
        use ciborium::value::{Integer, Value};
        self.check_breaker(&id)?;

        // Resolve the (possibly `frame=`-prefixed) locator to a node id,
        // descending into same-process iframes for cross-origin reach.
        let node = match self
            .resolve_locator_node(&id, session_id, target_id, &selector, budget_ms)
            .await
            .inspect_err(|_| self.record_failure(&id, FailureClass::Transport))?
        {
            Some(n) => n,
            None => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::SelectorNotFound);
            }
        };

        // Scroll into view (best-effort) before resolving coordinates.
        let _ = self
            .cdp_send_one(
                &id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.scrollIntoViewIfNeeded".into(),
                    params: Value::Map(vec![(
                        Value::Text("nodeId".into()),
                        Value::Integer(Integer::from(node)),
                    )]),
                },
                budget_ms,
            )
            .await?;

        // Box model → content-quad center. A CDP error here means the element
        // has no box model (display:none / detached) → NotHittable.
        let box_payload = match self
            .cdp_send_one(
                &id,
                session_id,
                target_id,
                CdpMessage {
                    method: "DOM.getBoxModel".into(),
                    params: Value::Map(vec![(
                        Value::Text("nodeId".into()),
                        Value::Integer(Integer::from(node)),
                    )]),
                },
                budget_ms,
            )
            .await?
        {
            Ok(p) => p,
            Err(_) => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::NotHittable);
            }
        };
        let (cx, cy) = match content_quad_center(&box_payload) {
            Some(c) => c,
            None => {
                self.record_success(&id);
                return Ok(InputDispatchOutcome::NotHittable);
            }
        };

        // Trusted click: mouseMoved → mousePressed → mouseReleased at (cx, cy).
        let events = vec![
            mouse_event("mouseMoved", cx, cy, "none", 0),
            mouse_event("mousePressed", cx, cy, "left", 1),
            mouse_event("mouseReleased", cx, cy, "left", 1),
        ];
        // commit_from_index = 1: `mousePressed` (index 1, after `mouseMoved`) is
        // the event that commits the click + can trigger the navigation whose
        // swap swallows the ack. A recv timeout there ⇒ the click WAS performed
        // (`DispatchedAckPending`). A pre-commit `mouseMoved` (index 0) timeout no
        // longer aborts: a swap that opens at/before the move swallows that ack
        // too, so `dispatch_input_events` keeps dispatching the committing frames
        // and reports the performed-but-swapped click as `DispatchedAckPending`.
        match self
            .dispatch_input_events(&id, session_id, target_id, events, budget_ms, 1)
            .await
        {
            Ok(ack) => {
                self.record_success(&id);
                Ok(ack.into_outcome())
            }
            Err(e) => {
                self.record_failure(&id, FailureClass::Application);
                Err(e)
            }
        }
    }
}

/// Center of a CDP `DOM.getBoxModel` content quad. `content` is
/// `[x1,y1,x2,y2,x3,y3,x4,y4]`; center = midpoint of opposite corners
/// (1 and 3). `None` when the payload lacks a usable quad.
fn content_quad_center(payload: &ciborium::value::Value) -> Option<(i64, i64)> {
    use ciborium::value::Value;
    let num = |v: &Value| -> Option<f64> {
        match v {
            Value::Float(f) => Some(*f),
            Value::Integer(i) => i64::try_from(*i).ok().map(|n| n as f64),
            _ => None,
        }
    };
    let content = cbor_get(payload, "model").and_then(|m| cbor_get(m, "content"))?;
    if let Value::Array(pts) = content {
        if pts.len() >= 6 {
            let x1 = num(&pts[0])?;
            let y1 = num(&pts[1])?;
            let x3 = num(&pts[4])?;
            let y3 = num(&pts[5])?;
            return Some((
                ((x1 + x3) / 2.0).round() as i64,
                ((y1 + y3) / 2.0).round() as i64,
            ));
        }
    }
    None
}

// ─── text=/role= marker resolver (P2, top frame) ─────────────────────────────
//
// We mark the matched element with a transient attribute and `querySelector` it
// (rather than returning coords) so the existing nodeId-based click/focus paths
// are reused unchanged. The marker is stripped after resolution. Resolution
// happens at record time only; replay is structural, so the marker never enters
// the hash chain.

const MARKER_ATTR: &str = "data-loom-loc";
const MARKER_SELECTOR: &str = "[data-loom-loc]";

/// Shared JS: clear any stale marker, plus `vis()` (visible: non-zero box and no
/// display:none/visibility:hidden) and `norm()` (collapse whitespace + trim).
const JS_PRELUDE: &str = "var M='data-loom-loc';document.querySelectorAll('['+M+']').forEach(function(e){e.removeAttribute(M);});function vis(e){var r=e.getBoundingClientRect();if(r.width===0&&r.height===0)return false;var s=getComputedStyle(e);return s.display!=='none'&&s.visibility!=='hidden';}function norm(t){return (t||'').replace(/\\s+/g,' ').trim();}";

/// `<input type=…>` → the implicit ARIA role `role=` matches it by (a subset of
/// Playwright's table; a type absent here has no role). The date/time family is
/// a `textbox` — Playwright's role for them — so `role=textbox[name="First day"]`
/// finds a native `<input type="date">`. number/range/file/image deliberately
/// stay role-less: giving file/image `button` would let a styled-away upload
/// input win a `role=button` selector (the resolver keeps the shortest name).
const INPUT_TYPE_ROLES: &[(&str, &str)] = &[
    ("text", "textbox"),
    ("email", "textbox"),
    ("password", "textbox"),
    ("search", "textbox"),
    ("tel", "textbox"),
    ("url", "textbox"),
    ("date", "textbox"),
    ("datetime-local", "textbox"),
    ("month", "textbox"),
    ("time", "textbox"),
    ("week", "textbox"),
    ("color", "textbox"),
    ("checkbox", "checkbox"),
    ("radio", "radio"),
    ("button", "button"),
    ("submit", "button"),
    ("reset", "button"),
];

/// W3C-AccName subset: implicit role mapping (`roleOf`, its `<input>` roles from
/// [`INPUT_TYPE_ROLES`]) + accessible-name computation (`accName`: aria-label →
/// aria-labelledby → associated label/placeholder → text → title).
fn role_helpers_js() -> &'static str {
    static JS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    JS.get_or_init(|| {
        let input_roles: serde_json::Map<String, serde_json::Value> = INPUT_TYPE_ROLES
            .iter()
            .map(|(ty, role)| ((*ty).to_string(), serde_json::Value::from(*role)))
            .collect();
        let input_roles = serde_json::Value::Object(input_roles).to_string();
        format!("var INPUT_ROLES={input_roles};{ROLE_OF_JS}{ACC_NAME_JS}")
    })
}

const ROLE_OF_JS: &str = "function roleOf(e){var r=e.getAttribute('role');if(r)return r.trim().toLowerCase();var tag=e.tagName.toLowerCase();if(tag==='button')return 'button';if(tag==='a'&&e.hasAttribute('href'))return 'link';if(tag==='select')return 'combobox';if(tag==='textarea')return 'textbox';if(/^h[1-6]$/.test(tag))return 'heading';if(tag==='input'){var ty=(e.getAttribute('type')||'text').toLowerCase();return Object.prototype.hasOwnProperty.call(INPUT_ROLES,ty)?INPUT_ROLES[ty]:'';}return '';}";

const ACC_NAME_JS: &str = "function accName(e){var al=e.getAttribute('aria-label');if(al&&al.trim())return norm(al);var lb=e.getAttribute('aria-labelledby');if(lb){var txt=lb.split(/\\s+/).map(function(id){var t=document.getElementById(id);return t?t.textContent:'';}).join(' ');if(norm(txt))return norm(txt);}var tag=e.tagName.toLowerCase();if(tag==='input'||tag==='textarea'||tag==='select'){if(e.id){try{var lbl=document.querySelector('label[for=\"'+(window.CSS&&CSS.escape?CSS.escape(e.id):e.id)+'\"]');if(lbl&&norm(lbl.textContent))return norm(lbl.textContent);}catch(_e){}}var pl=e.getAttribute('placeholder');if(pl&&pl.trim())return pl.trim();}var tc=norm(e.textContent);if(tc)return tc;var ti=e.getAttribute('title');if(ti&&ti.trim())return ti.trim();return '';}";

fn wrap(body: &str) -> String {
    let mut s = String::from("(function(){");
    s.push_str(body);
    s.push_str("})()");
    s
}

/// JS resolver expression for a `text=`/`role=` segment, or `None` for others.
fn marker_resolver_js(seg: &Segment) -> Option<String> {
    match seg {
        Segment::Text(needle) => Some(text_resolver_js(needle)),
        Segment::Role(spec) => {
            let (role, name) = parse_role_spec(spec);
            Some(role_resolver_js(&role, name.as_deref()))
        }
        Segment::Css(_) | Segment::Frame(_) => None,
    }
}

/// Deepest *visible* element whose normalized text contains `needle` (case-
/// insensitive). Candidates are the visible, text-matching elements; an element is
/// disqualified when it *contains another candidate* — i.e. the real match is
/// nested deeper — so a full-width wrapper never wins over the tight control it
/// contains. Disqualification is visibility-gated on purpose: a hidden child (a
/// `display:none`/`visibility:hidden` twin, a `.sr-only` label, an inline
/// `<script>`/`<style>` whose code happens to contain the text) is not a
/// candidate, so it neither steals the click nor makes its visible parent
/// unresolvable. Among the remaining deepest candidates we prefer the shortest
/// normalized text (closest to an exact match), then the smallest bounding-box
/// area. This is Playwright `getByText()` semantics. (The old code ranked all
/// matches by `textContent` length, which tied a wrapper with its sole-text child;
/// pre-order + strict `<` then made the wrapper win, so `web.click` landed on its
/// empty center.)
fn text_resolver_js(needle: &str) -> String {
    let n = serde_json::to_string(needle).unwrap_or_else(|_| "\"\"".into());
    let mut body = String::from(JS_PRELUDE);
    body.push_str("var needle=norm(");
    body.push_str(&n);
    body.push_str(").toLowerCase();if(!needle)return false;");
    body.push_str("var all=document.querySelectorAll('body *'),cand=[];for(var i=0;i<all.length;i++){var e=all[i];if(vis(e)&&norm(e.textContent).toLowerCase().indexOf(needle)!==-1)cand.push(e);}var best=null,bestLen=Infinity,bestArea=Infinity;for(var i=0;i<cand.length;i++){var e=cand[i],inner=false;for(var j=0;j<cand.length;j++){if(j!==i&&e.contains(cand[j])){inner=true;break;}}if(inner)continue;var len=norm(e.textContent).length,r=e.getBoundingClientRect(),area=r.width*r.height;if(len<bestLen||(len===bestLen&&area<bestArea)){best=e;bestLen=len;bestArea=area;}}if(best){best.setAttribute(M,'1');return true;}return false;");
    wrap(&body)
}

/// First *visible* element whose computed ARIA role equals `role` and (when
/// `name` is given) whose accessible name contains it (case-insensitive).
fn role_resolver_js(role: &str, name: Option<&str>) -> String {
    let role_j = serde_json::to_string(&role.to_lowercase()).unwrap_or_else(|_| "\"\"".into());
    let name_j = match name {
        Some(n) => serde_json::to_string(&n.to_lowercase()).unwrap_or_else(|_| "\"\"".into()),
        None => "null".into(),
    };
    let mut body = String::from(JS_PRELUDE);
    body.push_str(role_helpers_js());
    body.push_str("var wantRole=");
    body.push_str(&role_j);
    body.push_str(";var wantName=");
    body.push_str(&name_j);
    body.push_str(";var best=null,bestLen=1e9,all=document.querySelectorAll('body *');for(var i=0;i<all.length;i++){var e=all[i];if(!vis(e))continue;if(roleOf(e)!==wantRole)continue;var an=norm(accName(e)).toLowerCase();if(wantName!==null&&an.indexOf(wantName)===-1)continue;if(an.length<bestLen){best=e;bestLen=an.length;}}if(best){best.setAttribute(M,'1');return true;}return false;");
    wrap(&body)
}

/// Split `role=` value `NAME[name="X"]` into `("name"…, Some("X"))`. Tolerates
/// single/double quotes and an unquoted value terminated by `]`/space.
fn parse_role_spec(spec: &str) -> (String, Option<String>) {
    match spec.find('[') {
        Some(br) => {
            let role = spec[..br].trim().to_string();
            let rest = &spec[br..];
            let name = rest.find("name=").map(|i| &rest[i + 5..]).map(|s| {
                let s = s.trim_start();
                let (quote, s) = match s.chars().next() {
                    Some(q @ ('"' | '\'')) => (Some(q), &s[1..]),
                    _ => (None, s),
                };
                let end = match quote {
                    Some(q) => s.find(q).unwrap_or(s.len()),
                    None => s.find([']', ' ']).unwrap_or(s.len()),
                };
                s[..end].to_string()
            });
            (role, name)
        }
        None => (spec.trim().to_string(), None),
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
mod locator_resolver_tests {
    use super::*;

    #[test]
    fn parse_role_spec_extracts_role_and_quoted_name() {
        assert_eq!(
            parse_role_spec(r#"button[name="Send"]"#),
            ("button".into(), Some("Send".into()))
        );
        assert_eq!(parse_role_spec("button"), ("button".into(), None));
        assert_eq!(
            parse_role_spec("textbox[name='Email']"),
            ("textbox".into(), Some("Email".into()))
        );
    }

    #[test]
    fn text_resolver_embeds_needle_safely() {
        // A needle with a quote must not break out of the JS string literal.
        let js = text_resolver_js(r#"a"b"#);
        assert!(
            js.contains(r#""a\"b""#),
            "needle must be JSON-escaped: {js}"
        );
        assert!(js.starts_with("(function(){") && js.ends_with("})()"));
    }

    #[test]
    fn text_resolver_disqualifies_wrapper_via_nested_visible_candidate() {
        let js = text_resolver_js("Continue");
        // Candidacy is visibility-gated: a hidden child must neither be clicked nor
        // disqualify its visible parent.
        assert!(
            js.contains("vis(e)"),
            "candidates must be visibility-gated: {js}"
        );
        // A wrapper is disqualified when it CONTAINS another (visible) candidate, so
        // the click lands on the tight control, not the wrapper's empty center.
        assert!(
            js.contains("e.contains("),
            "must disqualify ancestors that contain a nested visible match: {js}"
        );
        // Among deepest candidates, prefer the closest-to-exact text then the
        // tightest box — not a global smallest-area pick.
        assert!(
            js.contains("bestLen")
                && js.contains("bestArea")
                && js.contains("getBoundingClientRect"),
            "must rank by shortest text then bounding-box area: {js}"
        );
    }

    #[test]
    fn role_resolver_includes_accname_helpers() {
        let js = role_resolver_js("button", Some("Continue"));
        assert!(js.contains("function accName"));
        assert!(
            js.contains("\"continue\""),
            "name lowercased + embedded: {js}"
        );
    }

    fn input_role(input_type: &str) -> Option<&'static str> {
        INPUT_TYPE_ROLES
            .iter()
            .find(|(ty, _)| *ty == input_type)
            .map(|(_, role)| *role)
    }

    #[test]
    fn date_family_inputs_are_textboxes() {
        for ty in ["date", "datetime-local", "month", "time", "week", "color"] {
            assert_eq!(input_role(ty), Some("textbox"), "{ty}");
        }
    }

    #[test]
    fn input_roles_are_otherwise_unchanged() {
        for ty in ["text", "email", "password", "search", "tel", "url"] {
            assert_eq!(input_role(ty), Some("textbox"), "{ty}");
        }
        assert_eq!(input_role("checkbox"), Some("checkbox"));
        assert_eq!(input_role("radio"), Some("radio"));
        for ty in ["button", "submit", "reset"] {
            assert_eq!(input_role(ty), Some("button"), "{ty}");
        }
        // Deliberately role-less: a file/image input as `button` could win a
        // `role=button` selector over the visible control.
        for ty in ["number", "range", "file", "image", "hidden"] {
            assert_eq!(input_role(ty), None, "{ty}");
        }
    }

    #[test]
    fn role_helpers_embed_the_whole_input_role_table() {
        let js = role_helpers_js();
        let start = js.find("var INPUT_ROLES=").expect("table declared") + "var INPUT_ROLES=".len();
        let end = start + js[start..].find(';').expect("table terminated");
        let table: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&js[start..end]).expect("table is JSON");
        assert_eq!(table.len(), INPUT_TYPE_ROLES.len());
        for (ty, role) in INPUT_TYPE_ROLES {
            assert_eq!(table.get(*ty).and_then(|r| r.as_str()), Some(*role), "{ty}");
        }
        assert!(js.contains("function roleOf") && js.contains("function accName"));
    }

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
