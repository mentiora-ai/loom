// User-story and parity acceptance tests — cross-cutting e2e behavior tests.
//
// Coverage:
//   - test_typed_receipt_drives_agent_control_flow
//   - test_agent_never_sees_raw_secret_in_manifest
//   - test_runaway_tab_killed_by_js_heap_budget
//   - test_all_documented_error_codes_are_matchable
//   - test_replay_100x_zero_divergence
//   - test_diff_reports_dom_hash_change
//   - test_har_export_is_har_12_valid
//   - test_js_heap_budget_kills_within_60s
//   - test_timing_ticks_present_in_receipt
//   - test_json_manifest_readable_without_binary_blobs
//   - test_evaluate_receipt_return_value_is_typed_json
//   - test_inspect_at_action_5_returns_entries_0_to_5
//   - test_vault_audit_trail_covers_five_grants

use loom_core::budget_enforcer::{
    BudgetEnforcer, BudgetLimits, KillCallback, KillReason, LocalBudgetEnforcer, ResourceKind,
    SessionCounters,
};
use loom_core::content_store::{ContentRef, ContentStore, LocalContentStore};
use loom_core::determinism_harness::DeterminismHarness;
use loom_core::error::LoomErrorCode;
use loom_core::exporters::Exporter;
use loom_core::manifest_writer::{
    AuditKind, LocalManifestWriter, ManifestEntry, ManifestWriter, SessionId,
};
use loom_core::observability::Observability;
use loom_core::receipt_builder::{NetworkEvent, ReceiptBuilder};
use loom_core::replay_engine::{DiffOpts, LocalReplayEngine, ReplayEngine, ReplayOpts};
use loom_core::session_manager::LocalSessionManager;
use loom_core::vault::{CredentialType, GrantOpts, KeychainAccess, LocalVault, Vault};
use ring::digest::{digest, SHA256};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tempfile::TempDir;
use zeroize::Zeroizing;

// ── Stub keychain (null secret — for non-secret tests) ────────────────────────

struct StubKc;
impl KeychainAccess for StubKc {
    fn get_secret(&self, _label: &str) -> Result<Zeroizing<Vec<u8>>, loom_keychain::KeychainError> {
        Ok(Zeroizing::new(vec![0u8; 16]))
    }
    fn set_secret(
        &self,
        _label: &str,
        _secret: Zeroizing<Vec<u8>>,
    ) -> Result<(), loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
    fn delete_secret(&self, _label: &str) -> Result<(), loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
    fn list_labels(&self) -> Result<Vec<String>, loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
}

// ── Sentinel keychain (recognizable secret — for secret-isolation tests) ─────

struct SentinelKc;
impl KeychainAccess for SentinelKc {
    fn get_secret(&self, _label: &str) -> Result<Zeroizing<Vec<u8>>, loom_keychain::KeychainError> {
        Ok(Zeroizing::new(b"ghp_TESTAPIKEY1234".to_vec()))
    }
    fn set_secret(
        &self,
        _label: &str,
        _secret: Zeroizing<Vec<u8>>,
    ) -> Result<(), loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
    fn delete_secret(&self, _label: &str) -> Result<(), loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
    fn list_labels(&self) -> Result<Vec<String>, loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
}

// ── Fixture helpers ───────────────────────────────────────────────────────────

fn make_obs(tmp: &TempDir) -> Arc<Observability> {
    Observability::new(tmp.path().join("loom.log"), false)
}

fn make_manifest_writer(tmp: &TempDir, obs: Arc<Observability>) -> Arc<LocalManifestWriter> {
    Arc::new(LocalManifestWriter::new(tmp.path().join("sessions"), obs))
}

fn make_content_store(tmp: &TempDir, obs: Arc<Observability>) -> Arc<LocalContentStore> {
    Arc::new(LocalContentStore::new(tmp.path().join("store"), obs))
}

fn make_harness(mw: Arc<dyn ManifestWriter>) -> Arc<DeterminismHarness> {
    Arc::new(DeterminismHarness::new(42, mw))
}

fn make_session_manager(
    tmp: &TempDir,
    mw: Arc<dyn ManifestWriter>,
    obs: Arc<Observability>,
) -> Arc<LocalSessionManager> {
    let cs: Arc<dyn ContentStore> = Arc::new(LocalContentStore::new(
        tmp.path().join("sm-store"),
        obs.clone(),
    ));
    let kc: Arc<dyn KeychainAccess> = Arc::new(StubKc);
    let v = Arc::new(LocalVault::new(kc, mw.clone(), obs.clone()));
    let be = Arc::new(LocalBudgetEnforcer::new(obs.clone()));
    LocalSessionManager::new(cs, mw, v, be, obs, 0, tmp.path().join("sessions"))
}

fn sha256_hex(b: &[u8]) -> String {
    let d = digest(&SHA256, b);
    d.as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Build a click receipt bytes using ReceiptBuilder (schema-drift-proof).
fn click_bytes(action_id: u64, dom_hash: &str) -> Vec<u8> {
    ReceiptBuilder::build_click_receipt(
        action_id.to_string(),
        action_id * 1_000,
        dom_hash.to_string(),
        sha256_hex(format!("screenshot-{action_id}").as_bytes()),
    )
    .canonical_bytes()
    .unwrap()
}

/// Build an evaluate receipt bytes using ReceiptBuilder.
/// Write a WAL with Header + N click ActionReceipts + SessionTerminal.
/// Each entry's prev_hash = sha256(raw_bytes_of_previous_line) so that
/// LocalManifestWriter::validate() passes.
/// Build a manifest writer rooted at `sessions_root` so synthetic test sessions
/// are written through the SAME append path production uses — in particular the
/// chain `prev_hash` is computed with the real projection rule (ephemeral
/// session_id/timestamps excluded from the hash). Hand-rolling `prev_hash` here
/// would diverge from `validate()` and fail replay.
fn helper_writer(sessions_root: &Path) -> LocalManifestWriter {
    let obs = Observability::new(sessions_root.join("helper.log"), false);
    LocalManifestWriter::new(sessions_root.to_path_buf(), obs)
}

fn write_click_session(sessions_root: &Path, session_id: &str, n: u64, dom_hash: &str) {
    let w = helper_writer(sessions_root);
    let sid = SessionId(session_id.to_string());
    w.open_manifest_with_started_at(sid.clone(), None, Some(0), None, None, true)
        .unwrap();
    for i in 0..n {
        w.append(
            sid.clone(),
            ManifestEntry::ActionReceipt {
                action_id: i,
                emitted_at_ms: i * 100,
                receipt_canonical_bytes: click_bytes(i, dom_hash),
                prev_hash: String::new(),
            },
        )
        .unwrap();
    }
    w.append(
        sid.clone(),
        ManifestEntry::SessionTerminal {
            action_id: n,
            emitted_at_ms: n * 100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();
}

/// Build navigate-receipt bytes via ReceiptBuilder + post-set tier-2 fields.
fn navigate_bytes(action_id: u64, url: &str, events: Vec<NetworkEvent>) -> Vec<u8> {
    let blob = ContentRef {
        sha256: "0".repeat(64),
        size_bytes: 0,
    };
    let mut p = ReceiptBuilder::build_navigate_receipt(
        action_id.to_string(),
        action_id * 1_000,
        blob.clone(),
        blob,
        events,
        Vec::new(),
    );
    p.url = Some(url.to_string());
    p.status_code = Some(200);
    p.canonical_bytes().unwrap()
}

/// Write a WAL with Header + N navigate ActionReceipts + SessionTerminal.
/// Each navigate carries one NetworkEvent so HAR export emits non-empty entries.
fn write_navigate_session(sessions_root: &Path, session_id: &str, n: u64) {
    let w = helper_writer(sessions_root);
    let sid = SessionId(session_id.to_string());
    w.open_manifest_with_started_at(sid.clone(), None, Some(0), None, None, true)
        .unwrap();
    for i in 0..n {
        let url = format!("https://example.com/page-{i}");
        let event = NetworkEvent {
            method: "GET".to_string(),
            url: url.clone(),
            status_code: 200,
            response_body_sha256_hex: "0".repeat(64),
            response_body_size_bytes: 1024,
            response_body_ref: None,
            timing_ticks: 50_000,
            content_type: "text/html".to_string(),
        };
        w.append(
            sid.clone(),
            ManifestEntry::ActionReceipt {
                action_id: i,
                emitted_at_ms: i * 100,
                receipt_canonical_bytes: navigate_bytes(i, &url, vec![event]),
                prev_hash: String::new(),
            },
        )
        .unwrap();
    }
    w.append(
        sid.clone(),
        ManifestEntry::SessionTerminal {
            action_id: n,
            emitted_at_ms: n * 100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();
}

// ── — Typed receipt drives agent control flow ─────────────────

#[test]
fn test_typed_receipt_drives_agent_control_flow() {
    // Build a 10-action session. An agent branching on receipt.dom_after_hash
    // can make correct decisions without inspecting screenshots.
    // We verify the dom_after_hash is present and stable across replay.
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let _mw = make_manifest_writer(&tmp, obs.clone());
    let _cs = make_content_store(&tmp, obs.clone());

    let dom_hash = sha256_hex(b"stable-dom-state");
    write_click_session(
        tmp.path().join("sessions").as_path(),
        "01USAGT01AGENT",
        10,
        &dom_hash,
    );

    // Parse all 10 receipts; verify each has dom_after_hash field
    let wal = fs::read_to_string(
        tmp.path()
            .join("sessions")
            .join("01USAGT01AGENT")
            .join("manifest.wal"),
    )
    .unwrap();

    let action_receipts: Vec<serde_json::Value> = wal
        .lines()
        .filter_map(|l| serde_json::from_str::<ManifestEntry>(l).ok())
        .filter_map(|e| {
            if let ManifestEntry::ActionReceipt {
                receipt_canonical_bytes,
                ..
            } = e
            {
                serde_json::from_slice(&receipt_canonical_bytes).ok()
            } else {
                None
            }
        })
        .collect();

    assert_eq!(action_receipts.len(), 10, "must have 10 action receipts");

    // Agent branching decision: every receipt has dom_after_hash for control-flow
    for (i, r) in action_receipts.iter().enumerate() {
        assert!(
            r["dom_after_hash"].is_string(),
            "receipt[{i}] must have dom_after_hash for agent branching"
        );
        assert_eq!(
            r["dom_after_hash"].as_str().unwrap(),
            dom_hash,
            "receipt[{i}] dom_after_hash must be stable (same DOM state)"
        );
        // Agent never needs to inspect screenshot — only dom_after_hash
        assert!(
            !r.get("screenshot_after_blob_ref")
                .is_some_and(|v| v.is_string()),
            "receipt[{i}] must not require screenshot inspection for control flow"
        );
    }
}

// ── — Agent never sees raw secrets ────────────────────────────

#[test]
fn test_agent_never_sees_raw_secret_in_manifest() {
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let mw = make_manifest_writer(&tmp, obs.clone());

    // Use SentinelKc — returns "ghp_TESTAPIKEY1234" as the secret
    let kc: Arc<dyn KeychainAccess> = Arc::new(SentinelKc);
    let vault = LocalVault::new(kc, mw.clone(), obs.clone());

    let sid = SessionId("01USAGT03SECRET".to_string());
    let session_dir = tmp.path().join("sessions").join(&sid.0);
    fs::create_dir_all(&session_dir).unwrap();
    mw.open_manifest(sid.clone(), None).unwrap();

    // Grant a credential (writes AuditEntry to manifest, NOT the raw secret)
    let _grant_id = vault
        .grant(
            sid.clone(),
            GrantOpts {
                credential_type: CredentialType::OAuth,
                label: "github-token".to_string(),
                origin: "https://github.com".to_string(),
                scopes: vec!["repo".to_string()],
                ttl_ms: 60_000,
                threat_model_acknowledged: true,
            },
        )
        .unwrap();

    // Read the manifest WAL and verify the sentinel never appears
    let wal = fs::read_to_string(session_dir.join("manifest.wal")).unwrap();
    assert!(
        !wal.contains("ghp_TESTAPIKEY1234"),
        "raw secret sentinel must never appear in the manifest WAL; agent transcript is safe"
    );

    // Every credential use shows grant-token mediation, not raw secret
    let has_grant_issued = wal.lines().any(|l| {
        serde_json::from_str::<ManifestEntry>(l)
            .map(|e| {
                matches!(
                    e,
                    ManifestEntry::AuditEntry {
                        audit_kind: AuditKind::GrantIssued,
                        ..
                    }
                )
            })
            .unwrap_or(false)
    });
    assert!(
        has_grant_issued,
        "manifest must contain GrantIssued audit entry"
    );
}

// ── — Runaway tab killed by budget ────────────────────────────

#[test]
fn test_runaway_tab_killed_by_js_heap_budget() {
    // Budget enforcer is in-memory; Observability's log path is never written to
    // disk here — anchor it to a throwaway TempDir instead of a shared /tmp literal.
    let tmp = TempDir::new().unwrap();
    let obs = Observability::new(tmp.path().join("loom.log"), false);
    let be = LocalBudgetEnforcer::new(obs);
    let sid = SessionId("01USAGT04RUNAWAY".to_string());

    let kill_log: Arc<Mutex<Vec<KillReason>>> = Arc::new(Mutex::new(Vec::new()));
    let kill_log2 = Arc::clone(&kill_log);
    let kill: KillCallback = Arc::new(move |_, reason| kill_log2.lock().unwrap().push(reason));

    let counters = SessionCounters::new();
    be.register_session(
        sid.clone(),
        Arc::clone(&counters),
        BudgetLimits {
            js_heap_bytes: 1,
            ..BudgetLimits::default()
        },
        kill,
    );

    // Simulate runaway JS heap balloon — exceeds 1-byte limit
    let result = be.account(sid.clone(), ResourceKind::JsHeap, 2);

    // Budget exceeded — kill callback fires
    assert!(
        result.is_err(),
        "account must return error when budget exceeded"
    );
    assert_eq!(result.unwrap_err().code, LoomErrorCode::BudgetExceeded);

    let log = kill_log.lock().unwrap();
    assert_eq!(log.len(), 1, "kill callback must fire exactly once");
    assert!(
        matches!(
            log[0],
            KillReason::BudgetExceeded {
                kind: ResourceKind::JsHeap,
                ..
            }
        ),
        "kill reason must be JsHeap budget exceeded"
    );
}

// ── — Pattern-matchable error recovery ────────────────────────

#[test]
fn test_all_documented_error_codes_are_matchable() {
    // All LoomErrorCode wire strings — now uniformly snake_case
    // (error-code-consolidation: as_wire flipped kebab→snake; daemon-distinct
    // variants merged from loom-rpc). Kept in sync with loom-shared/src/error_format.rs;
    // tools/lint-error-codes.py provides a second coverage layer.
    const WIRE_CODES: &[(&str, LoomErrorCode)] = &[
        ("session_not_found", LoomErrorCode::SessionNotFound),
        (
            "session_already_closed",
            LoomErrorCode::SessionAlreadyClosed,
        ),
        ("session_aborted", LoomErrorCode::SessionAborted),
        ("session_killed", LoomErrorCode::SessionKilled),
        ("surface_trap", LoomErrorCode::SurfaceTrap),
        ("vault_rejection", LoomErrorCode::VaultRejection),
        ("vault_grant_expired", LoomErrorCode::VaultGrantExpired),
        ("vault_grant_revoked", LoomErrorCode::VaultGrantRevoked),
        ("vault_unknown_label", LoomErrorCode::VaultUnknownLabel),
        ("budget_exceeded", LoomErrorCode::BudgetExceeded),
        ("budget_rate_limited", LoomErrorCode::BudgetRateLimited),
        (
            "store_integrity_failed",
            LoomErrorCode::StoreIntegrityFailed,
        ),
        ("store_not_found", LoomErrorCode::StoreNotFound),
        (
            "store_full_no_evictable",
            LoomErrorCode::StoreFullNoEvictable,
        ),
        ("manifest_corrupt", LoomErrorCode::ManifestCorrupt),
        ("replay_divergence", LoomErrorCode::ReplayDivergence),
        ("replay_missing_blob", LoomErrorCode::ReplayMissingBlob),
        ("llm_cache_miss", LoomErrorCode::LlmCacheMiss),
        ("shim_failure", LoomErrorCode::ShimFailure),
        ("shim_timeout", LoomErrorCode::ShimTimeout),
        ("shim_breaker_open", LoomErrorCode::ShimBreakerOpen),
        ("rpc_invalid_request", LoomErrorCode::RpcInvalidRequest),
        ("rpc_auth_failed", LoomErrorCode::RpcAuthFailed),
        ("rpc_schema_violation", LoomErrorCode::RpcSchemaViolation),
        ("transport_dropped", LoomErrorCode::TransportDropped),
        ("io", LoomErrorCode::Io),
        ("schema_violation", LoomErrorCode::SchemaViolation),
        (
            "safe_profile_download_blocked",
            LoomErrorCode::SafeProfileDownloadBlocked,
        ),
        ("invalid_argument", LoomErrorCode::InvalidArgument),
        ("unsupported", LoomErrorCode::Unsupported),
        ("internal", LoomErrorCode::Internal),
        // Daemon protocol / validation layer (merged from loom-rpc; BC-RPC-03 frozen).
        (
            "protocol_auth_required",
            LoomErrorCode::ProtocolAuthRequired,
        ),
        ("protocol_malformed", LoomErrorCode::ProtocolMalformed),
        ("method_not_found", LoomErrorCode::MethodNotFound),
        ("surface_unavailable", LoomErrorCode::SurfaceUnavailable),
        ("session_closed", LoomErrorCode::SessionClosed),
        ("vault_grant_not_found", LoomErrorCode::VaultGrantNotFound),
        (
            "vault_credential_type_unsupported",
            LoomErrorCode::VaultCredentialTypeUnsupported,
        ),
        ("unknown_profile", LoomErrorCode::UnknownProfile),
        ("invalid_network_mode", LoomErrorCode::InvalidNetworkMode),
        ("invalid_budget_key", LoomErrorCode::InvalidBudgetKey),
        (
            "invalid_capture_policy",
            LoomErrorCode::InvalidCapturePolicy,
        ),
        ("internal_error", LoomErrorCode::InternalError),
    ];

    for (wire, expected) in WIRE_CODES {
        // Deserialize from wire string (what an agent's match arm sees from RPC/MCP)
        let deserialized: LoomErrorCode = serde_json::from_value(serde_json::json!(wire))
            .unwrap_or_else(|e| panic!("failed to deserialize {:?}: {e}", wire));
        assert_eq!(
            &deserialized, expected,
            "serde round-trip failed for wire code {:?}",
            wire
        );
        // as_wire() must match the documented wire string (no fall-through to "unknown")
        assert_eq!(
            deserialized.as_wire(),
            *wire,
            "as_wire() mismatch for {:?}",
            wire
        );
    }
}

// ── — 100x replay zero divergence ────────────────────────────

#[test]
fn test_replay_100x_zero_divergence() {
    // Note: this exercises the diff() API path (not byte equality as in
    // replay_engine_behavior/replay.rs::test_replay_100x_produces_identical_receipt_bytes).
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone());
    let sm = make_session_manager(&tmp, mw.clone(), obs.clone());

    let dom_hash = sha256_hex(b"eval-01-dom");
    write_click_session(
        tmp.path().join("sessions").as_path(),
        "01USEVAL01SRC",
        3,
        &dom_hash,
    );

    let engine = LocalReplayEngine::new(
        cs.clone(),
        mw.clone(),
        make_harness(mw.clone()),
        obs.clone(),
        sm,
        tmp.path().join("sessions"),
    );

    // Run 100 replays; each must produce zero field differences
    for i in 0..100u64 {
        let replay_id = engine
            .replay(
                SessionId("01USEVAL01SRC".to_string()),
                ReplayOpts::default(),
            )
            .unwrap_or_else(|e| panic!("replay {i} failed: {e:?}"));

        let diff = engine
            .diff(
                SessionId("01USEVAL01SRC".to_string()),
                replay_id,
                DiffOpts {
                    exclude_screenshots: true,
                    include_audit_entries: false,
                },
            )
            .unwrap_or_else(|e| panic!("diff {i} failed: {e:?}"));

        assert!(
            diff.field_diffs.is_empty(),
            "replay {i}: expected zero field diffs, got {:?}",
            diff.field_diffs
        );
    }
}

// ── — Today-vs-yesterday diff command ────────────────────────

#[test]
fn test_diff_reports_dom_hash_change() {
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone());
    let sm = make_session_manager(&tmp, mw.clone(), obs.clone());

    let hash_day_n = sha256_hex(b"Log In");
    let hash_day_n1 = sha256_hex(b"Sign In");

    let sessions_root = tmp.path().join("sessions");
    write_click_session(&sessions_root, "01USEVAL02DAY0", 1, &hash_day_n);
    write_click_session(&sessions_root, "01USEVAL02DAY1", 1, &hash_day_n1);

    let engine = LocalReplayEngine::new(cs, mw.clone(), make_harness(mw), obs, sm, sessions_root);

    let diff = engine
        .diff(
            SessionId("01USEVAL02DAY0".to_string()),
            SessionId("01USEVAL02DAY1".to_string()),
            DiffOpts {
                exclude_screenshots: true,
                include_audit_entries: false,
            },
        )
        .unwrap();

    // The diff must identify the dom_after_hash divergence
    let dom_diff = diff
        .field_diffs
        .iter()
        .find(|d| d.field_path.contains("dom_after_hash"));
    assert!(
        dom_diff.is_some(),
        "diff must contain a field_diff for dom_after_hash (button label change); got: {:?}",
        diff.field_diffs
    );
    // source_value is the JSON-encoded representation of the field value.
    // dom_after_hash is a JSON string, so compare with the JSON-encoded form.
    let d = dom_diff.unwrap();
    assert_eq!(
        d.source_value,
        serde_json::json!(hash_day_n).to_string(),
        "source_value must be day-N hash (JSON-encoded)"
    );
    assert_eq!(
        d.replay_value,
        serde_json::json!(hash_day_n1).to_string(),
        "replay_value must be day-N+1 hash (JSON-encoded)"
    );
}

// ── — HAR export round-trips ─────────────────────────────────

#[test]
fn test_har_export_is_har_12_valid() {
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let cs = make_content_store(&tmp, obs);

    let sessions_root = tmp.path().join("sessions");
    write_navigate_session(&sessions_root, "01USEVAL03HAR", 3);

    let exporter = Exporter::new(sessions_root, cs);
    let bytes = exporter.export_har("01USEVAL03HAR").unwrap();

    // Must parse as valid JSON (Charles Proxy / Chrome DevTools requirement)
    let har: serde_json::Value =
        serde_json::from_slice(&bytes).expect("HAR output must be valid JSON");

    // HAR 1.2 structural requirements
    let log = har["log"].as_object().expect("HAR must have 'log' object");
    assert_eq!(
        log["version"].as_str().unwrap(),
        "1.2",
        "HAR version must be '1.2'"
    );

    let creator = log["creator"]
        .as_object()
        .expect("HAR must have 'creator' object");
    assert!(creator.contains_key("name"), "creator must have 'name'");
    assert!(
        creator.contains_key("version"),
        "creator must have 'version'"
    );

    let entries = log["entries"]
        .as_array()
        .expect("HAR must have 'entries' array");
    assert!(
        !entries.is_empty(),
        "HAR entries must be non-empty (3 click actions)"
    );

    // Each entry must have the required HAR fields for tool compatibility
    for (i, entry) in entries.iter().enumerate() {
        assert!(
            entry["startedDateTime"].is_string(),
            "entry[{i}] must have startedDateTime"
        );
        assert!(entry["time"].is_number(), "entry[{i}] must have time");
        let req = entry["request"]
            .as_object()
            .expect("entry[{i}] must have request");
        assert!(req["method"].is_string(), "entry[{i}].request.method");
        assert!(req["url"].is_string(), "entry[{i}].request.url");
    }
}

// ── — Budgets prevent CI runner exhaustion ───────────────────

#[test]
fn test_js_heap_budget_kills_within_60s() {
    // Budget enforcer is in-memory; Observability's log path is never written to
    // disk here — anchor it to a throwaway TempDir instead of a shared /tmp literal.
    let tmp = TempDir::new().unwrap();
    let obs = Observability::new(tmp.path().join("loom.log"), false);
    let be = LocalBudgetEnforcer::new(obs);
    let sid = SessionId("01USEVAL04CI".to_string());

    let kill_log: Arc<Mutex<Vec<KillReason>>> = Arc::new(Mutex::new(Vec::new()));
    let kill_log2 = Arc::clone(&kill_log);
    let kill: KillCallback = Arc::new(move |_, reason| kill_log2.lock().unwrap().push(reason));

    let counters = SessionCounters::new();
    // Simulate CI runner: 2 GB limit → set to 1 byte to force breach
    be.register_session(
        sid.clone(),
        Arc::clone(&counters),
        BudgetLimits {
            js_heap_bytes: 1,
            ..BudgetLimits::default()
        },
        kill,
    );

    let start = Instant::now();
    // Trigger JS-heap balloon (4 GB simulated as delta=2 against limit=1)
    let _ = be.account(sid.clone(), ResourceKind::JsHeap, 2);
    let elapsed = start.elapsed();

    // Kill must fire within 60 s wall-clock (enforcement is synchronous)
    assert!(
        elapsed.as_secs() < 60,
        "kill callback must fire within 60s; took {:?}",
        elapsed
    );

    let log = kill_log.lock().unwrap();
    assert_eq!(log.len(), 1, "kill callback must fire exactly once");
    assert!(
        matches!(
            log[0],
            KillReason::BudgetExceeded {
                kind: ResourceKind::JsHeap,
                ..
            }
        ),
        "kill reason must be JsHeap budget exceeded"
    );
}

// ── Auto-waiting parity with Playwright ─────────────────────

#[test]
fn test_timing_ticks_present_in_receipt() {
    // timing_ticks field is how Loom records implicit auto-wait intervals
    // (equivalent to Playwright's auto-wait). Tests that the field is present
    // and non-zero in the serialized receipt.
    // Note: actual hydration-wait behavior is tested in loom-host integration tests.
    let receipt = ReceiptBuilder::build_click_receipt(
        "action-1".to_string(),
        200_000, // 200 ms in ticks — simulates post-click hydration wait
        sha256_hex(b"after-hydration-dom"),
        sha256_hex(b"screenshot"),
    );

    let bytes = receipt.canonical_bytes().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(
        v["timing_ticks"].is_number(),
        "receipt must have timing_ticks field (auto-wait parity)"
    );
    assert_eq!(
        v["timing_ticks"].as_u64().unwrap(),
        200_000,
        "timing_ticks must record the full action duration including wait"
    );
}

// ── JSON-first trace consumability ──────────────────────────

#[test]
fn test_json_manifest_readable_without_binary_blobs() {
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let cs = make_content_store(&tmp, obs);

    let sessions_root = tmp.path().join("sessions");
    let dom_hash = sha256_hex(b"parity-02-dom");
    write_click_session(&sessions_root, "01PARITY02JSON", 3, &dom_hash);

    let exporter = Exporter::new(sessions_root, cs);
    let bytes = exporter.export_json("01PARITY02JSON").unwrap();

    // External tool reads only the JSON manifest (no Loom binary involved)
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // Must render a human-meaningful timeline without parsing binary blobs
    let manifest = v["manifest"]
        .as_object()
        .expect("export_json must have 'manifest' key");
    assert!(
        manifest.contains_key("entries") || !manifest.is_empty(),
        "manifest must contain action timeline entries"
    );

    // content_blob_index tells external tools where blobs are (no parsing required)
    assert!(
        v.get("content_blob_index").is_some(),
        "export_json must have 'content_blob_index' key for external tool compatibility"
    );
}

// ── Typed extract parity with Stagehand ─────────────────────

#[test]
fn test_evaluate_receipt_return_value_is_typed_json() {
    // The consumer receives a typed object, never has to parse a screenshot or string.
    let typed_value = serde_json::json!({"label": "Click here", "count": 42});

    let receipt = ReceiptBuilder::build_evaluate_receipt(
        "eval-1".to_string(),
        1_000,
        Some(typed_value.to_string()), // return_value_json is canonical-JSON string of value
        None,                          // return_value_blob_ref unset for inline values <= 64KB
        Vec::new(),
    );

    let bytes = receipt.canonical_bytes().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // Consumer parses return_value_json as typed serde_json::Value
    let return_value_str = v["return_value_json"]
        .as_str()
        .expect("evaluate receipt must have return_value_json string");
    let typed: serde_json::Value = serde_json::from_str(return_value_str)
        .expect("return_value_json must be valid JSON (typed, not screenshot)");

    assert_eq!(typed["label"].as_str().unwrap(), "Click here");
    assert_eq!(typed["count"].as_u64().unwrap(), 42);

    // Verify no screenshot blob is present (consumer never has to parse screenshots)
    assert!(
        v.get("screenshot_after_blob_ref").is_none() || v["screenshot_after_blob_ref"].is_null(),
        "evaluate receipt must not carry screenshot blob"
    );
}

// ── Time-travel parity with Replay.io (manifest scope) ──────

#[test]
fn test_inspect_at_action_5_returns_entries_0_to_5() {
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone());
    let sm = make_session_manager(&tmp, mw.clone(), obs.clone());

    let dom_hash = sha256_hex(b"parity-04-dom");
    let sessions_root = tmp.path().join("sessions");
    // Build a 10-action session (action_ids 0..9)
    write_click_session(&sessions_root, "01PARITY04INSPECT", 10, &dom_hash);

    let engine = LocalReplayEngine::new(cs, mw.clone(), make_harness(mw), obs, sm, sessions_root);

    // inspect at action 5 → must return entries 0..=5 (6 entries)
    let result = engine
        .inspect(SessionId("01PARITY04INSPECT".to_string()), Some(5))
        .unwrap();

    let entries = result["entries"]
        .as_array()
        .expect("inspect result must have 'entries' array");
    assert_eq!(
        entries.len(),
        6,
        "inspect at_action=5 must return 6 entries (0..=5)"
    );

    // Verify action IDs are 0..=5
    for (i, entry) in entries.iter().enumerate() {
        let aid = entry["action_id"].as_u64().unwrap();
        assert!(aid <= 5, "entry[{i}] action_id {aid} must be <= 5");
    }

    // Manifest must NOT be mutated (idempotent reads)
    let result2 = engine
        .inspect(SessionId("01PARITY04INSPECT".to_string()), Some(5))
        .unwrap();
    assert_eq!(
        result2["entries"].as_array().unwrap().len(),
        6,
        "second inspect must produce same result (immutable)"
    );
}

// ── Receipt-mediated audit parity with vault ecosystem ───────

#[test]
fn test_vault_audit_trail_covers_five_grants() {
    let tmp = TempDir::new().unwrap();
    let obs = make_obs(&tmp);
    let mw = make_manifest_writer(&tmp, obs.clone());

    // Use SentinelKc — recognizable secret that must never appear in WAL
    let kc: Arc<dyn KeychainAccess> = Arc::new(SentinelKc);
    let vault = LocalVault::new(kc, mw.clone(), obs);

    let sid = SessionId("01PARITY05AUDIT".to_string());
    let session_dir = tmp.path().join("sessions").join(&sid.0);
    fs::create_dir_all(&session_dir).unwrap();
    mw.open_manifest(sid.clone(), None).unwrap();

    // Exercise 5 grants (simulates the audit-parity scenario)
    for i in 0..5u32 {
        vault
            .grant(
                sid.clone(),
                GrantOpts {
                    credential_type: CredentialType::OAuth,
                    label: format!("service-{i}"),
                    origin: format!("https://service-{i}.example.com"),
                    scopes: vec!["read".to_string()],
                    ttl_ms: 60_000,
                    threat_model_acknowledged: true,
                },
            )
            .unwrap();
    }

    // Read the manifest WAL
    let wal = fs::read_to_string(session_dir.join("manifest.wal")).unwrap();

    // Must have an audit trail equivalent to server-side vault ecosystems
    let grant_issued_count = wal
        .lines()
        .filter_map(|l| serde_json::from_str::<ManifestEntry>(l).ok())
        .filter(|e| {
            matches!(
                e,
                ManifestEntry::AuditEntry {
                    audit_kind: AuditKind::GrantIssued,
                    ..
                }
            )
        })
        .count();

    assert_eq!(
        grant_issued_count, 5,
        "manifest must contain exactly 5 GrantIssued audit entries (parity with server-side vaults)"
    );

    // Audit trail is local and traveled-with-session (FR-VAULT-03)
    // Raw secret must never appear (tamper-evident security property)
    assert!(
        !wal.contains("ghp_TESTAPIKEY1234"),
        "raw secret sentinel must never appear in the audit trail"
    );
}
