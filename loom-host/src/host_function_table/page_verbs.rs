// Host function bodies for the page verbs the guest runs through typed host
// functions — `wait-for-execute`, `evaluate-execute`, `set-input-files-execute`;
// the trait methods in `host_impl.rs` delegate here. Moved verbatim.

use super::host_function_table::HostState;
use crate::host_function_table::chromium_registration::register_chromium_shim_if_absent;
use crate::host_function_table::host_impl::cbor_value_to_json;
use crate::host_function_table::host_impl::core_ref_to_wit;
use crate::host_function_table::host_impl::loom_to_wit_error;
use crate::wit_type_marshaller::loom_surface_bindings::loom::surface::{
    host::Host,
    types::{EvaluateResult, HostError, SetInputFilesResult, WaitResult},
};
use crate::wit_type_marshaller::Mode;

impl HostState {
    // --- Typed wait-for-execute host function (settle-capture slice 2) ---
    pub(super) fn wait_for_execute_impl(
        &mut self,
        action_id: String,
        until: String,
        budget_ms: u64,
    ) -> impl ::core::future::Future<Output = Result<WaitResult, HostError>> + Send + '_ {
        use crate::shim_manager::ShimId;

        // ALL sync work up front so the future holds only owned Send types.
        let session_id_str = self.session_id.0.clone();
        let shim_manager = self.shim_manager.clone();
        let determinism = self.determinism.clone();
        let seed = self.seed;
        let epoch_ms = self.epoch_ms;
        // settle-capture (4b): per-session determinism toggle.
        let determinism_enabled = !self.no_determinism;
        // voice-call-io: per-session audio opt-in, carried onto the wire so the
        // shim installs the mic bootstrap on the (lazy-spawned) target.
        let audio_enabled = self.audio;
        let shim_session_id = shim_manager.shim_session_id_for(&session_id_str);

        let maybe_id: Result<ShimId, HostError> =
            if self.mode == crate::wit_type_marshaller::Mode::Replay {
                Err(HostError::Internal(
                    "wait_for_execute not allowed in replay mode".to_owned(),
                ))
            } else {
                // wait_for never navigates or downloads, so it needs only the
                // common register core (no safe-profile downloads_dir env).
                register_chromium_shim_if_absent(&shim_manager, &session_id_str, |config| {
                    // voice-call-io: thread the --audio opt-in to the shim's
                    // supervisor (fake-media launch flags) at whichever verb
                    // registers the shim first. Mirrors the navigate site.
                    if audio_enabled {
                        config.env.push(("LOOM_SHIM_AUDIO".into(), "1".into()));
                    }
                })
            };

        async move {
            let effective_id = maybe_id?;

            // target_id = 0 → resolves to the session's current target (the
            // shim's WaitFor handler runs after an idempotent SpawnTarget).
            let outcome = shim_manager
                .send_wait_for(crate::shim_manager::SendWaitForParams {
                    id: effective_id,
                    action_id,
                    session_id: shim_session_id,
                    target_id: 0,
                    until,
                    budget_ms,
                    seed,
                    epoch_ms,
                    determinism_enabled,
                    audio_enabled,
                })
                .await
                .map_err(loom_to_wit_error)?;

            // Session-monotonic timestamp from DeterminismHarness (like navigate).
            let emitted_at_ms = self
                .deterministic_clock_ms()
                .unwrap_or_else(|| determinism.clock_now());

            Ok(WaitResult {
                settle_until: outcome.settle_until,
                settle_outcome: outcome.settle_outcome,
                settle_ms: outcome.settle_ms,
                network_count_at_settle: outcome.network_count_at_settle,
                emitted_at_ms,
            })
        }
    }

    pub(super) fn evaluate_execute_impl(
        &mut self,
        action_id: String,
        expression: String,
        budget_ms: u64,
    ) -> impl ::core::future::Future<Output = Result<EvaluateResult, HostError>> + Send + '_ {
        use crate::shim_manager::ShimId;

        let session_id_str = self.session_id.0.clone();
        let shim_manager = self.shim_manager.clone();
        let content_store = self.core.content_store.clone();
        let determinism = self.determinism.clone();
        // Capture seed + epoch_ms so send_evaluate can do a lazy
        // SpawnTarget under the same per-session determinism context as
        // navigate_execute. Without this, evaluate-without-prior-navigate
        // routes to the bootstrap about:blank context where Date.now /
        // Math.random leak real values.
        let seed = self.seed;
        let epoch_ms = self.epoch_ms;
        // settle-capture (4b): per-session determinism toggle.
        let determinism_enabled = !self.no_determinism;
        // voice-call-io: per-session audio opt-in, carried onto the wire so the
        // shim installs the mic bootstrap on the (lazy-spawned) target.
        let audio_enabled = self.audio;
        let shim_session_id = shim_manager.shim_session_id_for(&session_id_str);

        // Resolve effective shim ID and lazy-register if needed (same
        // pattern as navigate_execute).
        let maybe_id: Result<ShimId, HostError> =
            if self.mode == crate::wit_type_marshaller::Mode::Replay {
                Err(HostError::Internal(
                    "evaluate_execute not allowed in replay mode".to_owned(),
                ))
            } else {
                register_chromium_shim_if_absent(&shim_manager, &session_id_str, |config| {
                    // voice-call-io: thread the --audio opt-in to the shim's
                    // supervisor (fake-media launch flags) at whichever verb
                    // registers the shim first. Mirrors the navigate site.
                    if audio_enabled {
                        config.env.push(("LOOM_SHIM_AUDIO".into(), "1".into()));
                    }
                })
            };

        async move {
            let effective_id = maybe_id?;

            // target_id = 0 (current stub) — send_evaluate will lazy-spawn
            // the determinism-injected target via SpawnTarget before
            // dispatching the actual Runtime.evaluate.
            let outcome = shim_manager
                .send_evaluate(crate::shim_manager::SendEvaluateParams {
                    id: effective_id,
                    action_id,
                    session_id: shim_session_id,
                    target_id: 0,
                    expression,
                    budget_ms,
                    seed,
                    epoch_ms,
                    determinism_enabled,
                    audio_enabled,
                })
                .await
                .map_err(loom_to_wit_error)?;

            // Page-side throw → typed shaped error.
            if let Some(ex) = outcome.exception {
                let detail = serde_json::json!({
                    "kind": "js_throw",
                    "exception": ex.message,
                    "line": ex.line,
                    "column": ex.column,
                })
                .to_string();
                return Err(HostError::ShimFailure(detail));
            }

            // Success path: convert CBOR result → canonical-JSON.
            let cbor_value = outcome.result.ok_or_else(|| {
                HostError::Internal("evaluate: no result and no exception".into())
            })?;
            let json_value = cbor_value_to_json(cbor_value)?;
            let canonical_json = serde_jcs::to_string(&json_value).map_err(|e| {
                HostError::Internal(format!("evaluate: canonical-JSON serialization: {e}"))
            })?;

            // Session-monotonic timestamp.
            let emitted_at_ms = self
                .deterministic_clock_ms()
                .unwrap_or_else(|| determinism.clock_now());

            // > 64KB → offload to content store.
            const TRUNCATION_THRESHOLD: usize = 65_536;
            if canonical_json.len() > TRUNCATION_THRESHOLD {
                let cref = content_store
                    .put(canonical_json.as_bytes())
                    .map_err(loom_to_wit_error)?;
                Ok(EvaluateResult {
                    return_value_json: None,
                    return_value_blob_ref: Some(core_ref_to_wit(cref)),
                    emitted_at_ms,
                })
            } else {
                Ok(EvaluateResult {
                    return_value_json: Some(canonical_json),
                    return_value_blob_ref: None,
                    emitted_at_ms,
                })
            }
        }
    }

    pub(super) fn set_input_files_execute_impl(
        &mut self,
        action_id: String,
        payload: Vec<u8>,
        budget_ms: u64,
    ) -> impl ::core::future::Future<Output = Result<SetInputFilesResult, HostError>> + Send + '_
    {
        use crate::shim_manager::{SetInputFilesOutcome, ShimId};

        // The guest forwards the daemon-built canonical JSON
        // {"selector":..,"paths":[..]} verbatim (it has no JSON parser).
        // Parse it host-side. Malformed payload → Internal (should never
        // happen — the daemon builds it).
        #[derive(serde::Deserialize)]
        struct SetInputFilesPayload {
            selector: String,
            paths: Vec<String>,
        }
        let parsed: Result<SetInputFilesPayload, HostError> = serde_json::from_slice(&payload)
            .map_err(|e| HostError::Internal(format!("set_input_files: payload parse: {e}")));

        let session_id_str = self.session_id.0.clone();
        let shim_manager = self.shim_manager.clone();
        let seed = self.seed;
        let epoch_ms = self.epoch_ms;
        // settle-capture (4b): per-session determinism toggle.
        let determinism_enabled = !self.no_determinism;
        // voice-call-io: per-session audio opt-in, carried onto the wire so the
        // shim installs the mic bootstrap on the (lazy-spawned) target.
        let audio_enabled = self.audio;
        let shim_session_id = shim_manager.shim_session_id_for(&session_id_str);

        // Resolve effective shim ID and lazy-register if needed (same
        // pattern as navigate_execute / evaluate_execute).
        let maybe_id: Result<ShimId, HostError> = if self.mode == Mode::Replay {
            Err(HostError::Internal(
                "set_input_files_execute not allowed in replay mode".to_owned(),
            ))
        } else {
            register_chromium_shim_if_absent(&shim_manager, &session_id_str, |_config| {})
        };

        async move {
            let effective_id = maybe_id?;
            let SetInputFilesPayload { selector, paths } = parsed?;
            // Paths are already validated + canonicalized daemon-side
            // (upload_guard). target_id = 0 → resolves to the session target,
            // same as send_evaluate.
            let outcome = shim_manager
                .send_set_input_files(crate::shim_manager::SendSetInputFilesParams {
                    id: effective_id,
                    action_id,
                    session_id: shim_session_id,
                    target_id: 0,
                    selector,
                    files: paths,
                    budget_ms,
                    seed,
                    epoch_ms,
                    determinism_enabled,
                    audio_enabled,
                })
                .await
                .map_err(loom_to_wit_error)?;

            match outcome {
                SetInputFilesOutcome::Ok { file_count } => Ok(SetInputFilesResult { file_count }),
                // Discrete typed wire kinds (plan-council FND#8) — NOT panics.
                SetInputFilesOutcome::SelectorNotFound => Err(HostError::ShimFailure(
                    serde_json::json!({"kind":"selector_not_found","verb":"set_input_files"})
                        .to_string(),
                )),
                SetInputFilesOutcome::NotAFileInput => Err(HostError::ShimFailure(
                    serde_json::json!({"kind":"not_a_file_input","verb":"set_input_files"})
                        .to_string(),
                )),
            }
        }
    }
}
