//! Wire-level replay-refusal fidelity tests.
//!
//! Every replay refusal path must reach the client with its intended
//! canonical code AND its compiled-in human message — never a translator
//! default like `schema_violation: session.replay failed for session <SID>`
//! (the verified v0.10.1 bug for `--no-determinism` recordings).
//!
//! The scripted bridge below returns the EXACT `LoomError`s loom-core
//! emits on each refusal path (codes + message templates are pinned at the
//! source by loom-core/tests/replay_engine_behavior/refusal.rs and
//! loom-core/tests/integration.rs; the daemon bridge passes them through
//! untranslated — see `CoreBridge::replay_session_to_id`). Dispatch goes
//! through the real `CoreServiceAdapter` → `RpcHandlers` → `RequestRouter`
//! stack, and the asserted bytes are the exact JSON the connection handler
//! frames onto the socket.

use loom_rpc::{
    core_service_adapter::core_service_adapter::{
        CoreFacadeBridge, CoreServiceAdapter, ExportInfo, GcRunReport, GrantInfo, GrantParams,
        LoomError, PlaywrightImportInfo, ReapReport, ValidationResult, VaultAddInfo,
        VaultAddParams, VaultDeleteSecretInfo, VaultDeleteSecretParams, VaultDiagnoseInfo,
        VaultGetSessionContextInfo, VaultListLabelsInfo, VaultListLabelsParams, VaultSetSecretInfo,
        VaultSetSecretParams,
    },
    error_translator::error_translator::LoomErrorCode,
    host_service_adapter::host_service_adapter::{
        Action, HostServiceAdapter, Receipt, WasmHostBridge,
    },
    request_router::request_router::{RequestRouter, RequestRouterApi},
    rpc_handlers::rpc_handlers::RpcHandlers,
    rpc_observability::rpc_observability::RpcObservability,
    schema_provider::schema_provider::{CompiledJsonSchema, SchemaProviderApi, SchemaRegistry},
    schema_validator::schema_validator::SchemaValidator,
};
use std::sync::Arc;

/// The six refusal paths, scripted by session_id. Messages mirror the
/// loom-core emit sites verbatim (see module docs).
fn scripted_refusal(session_id: &str) -> LoomError {
    match session_id {
        "src-not-found" => LoomError::new(
            LoomErrorCode::SessionNotFound,
            "session src-not-found does not exist",
        ),
        "src-crashed" => LoomError::new(
            LoomErrorCode::SessionAborted,
            "session src-crashed crashed mid-flow; replay refuses to reproduce a partial trace",
        ),
        "src-aborted" => LoomError::new(
            LoomErrorCode::SessionAborted,
            "session src-aborted ended via abort (reason=user-initiated); \
             replay refuses to reproduce an abandoned trace",
        ),
        "src-missing-blob" => LoomError::new(
            LoomErrorCode::ReplayMissingBlob,
            "pre-flight: missing blob deadbeef (kind: dom_snapshot)",
        ),
        "src-broken-chain" => LoomError::new(
            LoomErrorCode::ManifestCorrupt,
            "hash chain broken at index 2",
        )
        .with_context(serde_json::json!({
            "failed_at_index": 2,
            "expected_hash": "aaaa",
            "expected_hash_legacy": "bbbb",
            "observed_hash": "cccc",
        })),
        "src-no-determinism" => LoomError::new(
            LoomErrorCode::NotReplayable,
            "session src-no-determinism was recorded with --no-determinism (real clock + \
             unseeded RNG) and is NOT replayable: a non-deterministic run can never be \
             replay-equal",
        ),
        // Degenerate: empty message → handler falls back to the generic template.
        "src-empty-message" => LoomError::new(LoomErrorCode::InternalError, ""),
        other => panic!("unscripted session_id: {other}"),
    }
}

struct ScriptedReplayBridge;

#[rustfmt::skip]
impl CoreFacadeBridge for ScriptedReplayBridge {
    fn replay_session_to_id(&self, session_id: &str) -> Result<String, LoomError> {
        Err(scripted_refusal(session_id))
    }
    fn list_sessions_info(&self) -> Result<Vec<(String, String, u64)>, LoomErrorCode> { Ok(vec![]) }
    fn export_session_to_cas(&self, _: &str, _: &str) -> Result<ExportInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn get_export_bytes(&self, _: &str) -> Result<Vec<u8>, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn diff_sessions_json(&self, _: &str, _: &str, _: bool) -> Result<serde_json::Value, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn inspect_session_json(&self, _: &str, _: Option<u64>) -> Result<serde_json::Value, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn validate_session_result(&self, session_id: &str) -> Result<ValidationResult, LoomErrorCode> {
        // PASS ≠ replayable: a --no-determinism session validates clean but
        // carries the replayable=false verdict + the replay refusal reason.
        Ok(ValidationResult {
            session_id: session_id.to_string(),
            passed: true,
            reasons: vec![],
            replayable: false,
            not_replayable_reason: Some(scripted_refusal("src-no-determinism").message),
        })
    }
    fn session_reap(&self, _: bool) -> Result<ReapReport, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn import_playwright_from_bytes(&self, _: &[u8]) -> Result<PlaywrightImportInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn create_session_raw(&self, _: loom_rpc::core_service_adapter::core_service_adapter::CreateSessionParams) -> Result<(String, u64), loom_shared::error_format::LoomError> { Err(LoomErrorCode::InternalError.into()) }
    fn close_session_raw(&self, _: &str) -> Result<(), LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn abort_session_raw(&self, _: &str, _: &str) -> Result<(), LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_grant(&self, _: GrantParams) -> Result<GrantInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_revoke(&self, _: &str, _: &str) -> Result<(), LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_list_grants(&self, _: Option<&str>) -> Result<Vec<GrantInfo>, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_add(&self, _: VaultAddParams) -> Result<VaultAddInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_set_secret(&self, _: VaultSetSecretParams) -> Result<VaultSetSecretInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_delete_secret(&self, _: VaultDeleteSecretParams) -> Result<VaultDeleteSecretInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_list_labels(&self, _: VaultListLabelsParams) -> Result<VaultListLabelsInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_diagnose(&self) -> Result<VaultDiagnoseInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn vault_get_session_context(&self) -> Result<VaultGetSessionContextInfo, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
    fn gc_run(&self, _: Option<u64>, _: Option<u64>) -> Result<GcRunReport, LoomErrorCode> { Err(LoomErrorCode::InternalError) }
}

struct StubHostBridge;
impl WasmHostBridge for StubHostBridge {
    fn dispatch_action_blocking(
        &self,
        _: Action,
        _: Option<u64>,
    ) -> Result<Receipt, LoomErrorCode> {
        Err(LoomErrorCode::SurfaceUnavailable)
    }
}

struct EmptySchemas;
impl SchemaProviderApi for EmptySchemas {
    fn lookup_request_schema(&self, _: &str) -> Option<Arc<CompiledJsonSchema>> {
        None
    }
    fn lookup_response_schema(&self, _: &str) -> Option<Arc<CompiledJsonSchema>> {
        None
    }
    fn registered_methods(&self) -> Vec<String> {
        vec!["session.replay".to_string()]
    }
    fn get_registry_snapshot(&self) -> SchemaRegistry {
        SchemaRegistry {
            methods: vec![],
            source_wit_sha256: String::new(),
        }
    }
}

fn make_router() -> Arc<RequestRouter> {
    let schemas: Arc<dyn SchemaProviderApi> = Arc::new(EmptySchemas);
    let validator = SchemaValidator::new(Arc::clone(&schemas));
    let obs = RpcObservability::new();
    let core = CoreServiceAdapter::new(Arc::new(ScriptedReplayBridge));
    let host = HostServiceAdapter::new(Arc::new(StubHostBridge));
    let handlers = RpcHandlers::new(core, host, Arc::clone(&schemas), validator.clone(), obs);
    RequestRouter::register_methods(handlers, schemas, validator)
        .expect("router registration must succeed")
}

/// Dispatch `session.replay` for `sid` and decode the wire error envelope.
async fn replay_error_envelope(router: &RequestRouter, sid: &str) -> serde_json::Value {
    let bytes = router
        .dispatch("session.replay", serde_json::json!({ "session_id": sid }))
        .await;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).expect("wire bytes must be valid JSON");
    assert_eq!(
        v.get("__loom_rpc_error"),
        Some(&serde_json::Value::Bool(true)),
        "refusal must be an error envelope, got: {v}"
    );
    v
}

/// One assertion per refusal path: intended canonical wire code + the full
/// human message, verbatim — not the pre-fix `schema_violation: session.replay
/// failed for session <SID>` default.
#[tokio::test]
async fn every_replay_refusal_path_reaches_the_wire_with_code_and_message() {
    let router = make_router();
    let cases: &[(&str, &str)] = &[
        ("src-not-found", "session_not_found"),
        ("src-crashed", "session_aborted"),
        ("src-aborted", "session_aborted"),
        ("src-missing-blob", "replay_missing_blob"),
        ("src-broken-chain", "manifest_corrupt"),
        ("src-no-determinism", "not_replayable"),
    ];
    for (sid, expected_code) in cases {
        let v = replay_error_envelope(&router, sid).await;
        assert_eq!(
            v["code"].as_str(),
            Some(*expected_code),
            "{sid}: wire code must be the intended canonical code, got: {v}"
        );
        let expected_message = scripted_refusal(sid).message;
        assert_eq!(
            v["message"].as_str(),
            Some(expected_message.as_str()),
            "{sid}: the compiled-in refusal message must reach the wire verbatim"
        );
    }
}

/// The `--no-determinism` refusal specifically: a determinism-specific code
/// (NOT `schema_violation`) + the full explanatory message, per the spec's
/// acceptance criteria.
#[tokio::test]
async fn no_determinism_refusal_is_not_replayable_not_schema_violation() {
    let router = make_router();
    let v = replay_error_envelope(&router, "src-no-determinism").await;
    assert_eq!(v["code"].as_str(), Some("not_replayable"));
    let msg = v["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("--no-determinism")
            && msg.contains("NOT replayable")
            && msg.contains("never be replay-equal"),
        "full compiled-in explanation must reach the wire; got: {msg}"
    );
}

/// Structured refusal context (`LoomError.context`) survives to the
/// envelope's `data` field — e.g. the broken-chain break-point details.
#[tokio::test]
async fn broken_chain_refusal_carries_structured_context_as_data() {
    let router = make_router();
    let v = replay_error_envelope(&router, "src-broken-chain").await;
    assert_eq!(v["code"].as_str(), Some("manifest_corrupt"));
    assert_eq!(
        v["data"]["failed_at_index"].as_u64(),
        Some(2),
        "LoomError.context must pass through to the wire data field; got: {v}"
    );
}

/// The generic template survives ONLY as a fallback for empty messages.
#[tokio::test]
async fn empty_refusal_message_falls_back_to_generic_template() {
    let router = make_router();
    let v = replay_error_envelope(&router, "src-empty-message").await;
    assert_eq!(v["code"].as_str(), Some("internal_error"));
    assert_eq!(
        v["message"].as_str(),
        Some("session.replay failed for session src-empty-message"),
        "empty message → generic template fallback"
    );
}

/// `session.validate` carries the `replayable` verdict (+reason) to the
/// wire: PASS alone no longer implies replayable. The serde-default keeps
/// the field additive (old clients ignore it; new clients decoding an
/// old daemon's payload default to `replayable: true`).
#[tokio::test]
async fn session_validate_carries_replayable_verdict_and_reason() {
    let router = make_router();
    let bytes = router
        .dispatch(
            "session.validate",
            serde_json::json!({ "session_id": "src-no-determinism" }),
        )
        .await;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).expect("wire bytes must be valid JSON");
    assert_eq!(v["passed"].as_bool(), Some(true), "integrity PASSes: {v}");
    assert_eq!(
        v["replayable"].as_bool(),
        Some(false),
        "PASS must not imply replayable for a --no-determinism session: {v}"
    );
    let reason = v["not_replayable_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("--no-determinism") && reason.contains("NOT replayable"),
        "reason must carry the replay refusal explanation; got: {reason}"
    );
}

/// Backward compat: an old-daemon payload WITHOUT the new fields decodes
/// with `replayable: true` (the pre-field meaning of PASS) and no reason.
#[test]
fn validation_result_serde_defaults_compose_with_old_payloads() {
    let old_payload = serde_json::json!({
        "session_id": "01OLD",
        "passed": true,
        "reasons": [],
    });
    let r: ValidationResult =
        serde_json::from_value(old_payload).expect("old payload must still decode");
    assert!(r.replayable, "missing field defaults to true (old meaning)");
    assert!(r.not_replayable_reason.is_none());
}
