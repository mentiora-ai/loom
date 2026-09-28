//! Shared harness: a stub keychain, the engine stack, and a recorded session.

use loom_core::budget_enforcer::{BudgetEnforcer, LocalBudgetEnforcer};
use loom_core::content_store::{ContentStore, LocalContentStore};
use loom_core::determinism_harness::DeterminismHarness;
use loom_core::manifest_writer::{LocalManifestWriter, ManifestEntry, ManifestWriter, SessionId};
use loom_core::observability::Observability;
use loom_core::replay_engine::LocalReplayEngine;
use loom_core::session_manager::LocalSessionManager;
use loom_core::vault::{KeychainAccess, LocalVault, Vault};
use ring::digest::{digest, SHA256};
use std::path::PathBuf;
use std::sync::Arc;
use zeroize::Zeroizing;

// ---- minimal Keychain stub for LocalVault construction ----

pub(crate) struct StubKc;
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

// ---- test harness helpers ----

pub(crate) fn tmp_path() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

pub(crate) fn make_obs(tmp: &tempfile::TempDir) -> Arc<Observability> {
    Observability::new(tmp.path().join("loom.log"), false)
}

pub(crate) fn make_manifest_writer(
    tmp: &tempfile::TempDir,
    obs: Arc<Observability>,
) -> Arc<LocalManifestWriter> {
    Arc::new(LocalManifestWriter::new(tmp.path().join("sessions"), obs))
}

pub(crate) fn make_harness(seed: u64, mw: Arc<dyn ManifestWriter>) -> Arc<DeterminismHarness> {
    Arc::new(DeterminismHarness::new(seed, mw))
}

pub(crate) fn make_content_store(
    tmp: &tempfile::TempDir,
    obs: Arc<Observability>,
) -> Arc<LocalContentStore> {
    Arc::new(LocalContentStore::new(tmp.path().join("store"), obs))
}

pub(crate) fn make_session_manager(
    tmp: &tempfile::TempDir,
    mw: Arc<dyn ManifestWriter>,
    // Sessions mint their own per-session DeterminismHarness at create()
    // now; the parameter is kept so the ~20 call sites stay untouched.
    _dh: Arc<DeterminismHarness>,
    obs: Arc<Observability>,
) -> Arc<LocalSessionManager> {
    let cs: Arc<dyn ContentStore> = Arc::new(LocalContentStore::new(
        tmp.path().join("store"),
        obs.clone(),
    ));
    let kc: Arc<dyn KeychainAccess> = Arc::new(StubKc);
    let v: Arc<dyn Vault> = Arc::new(LocalVault::new(kc, mw.clone(), obs.clone()));
    let be: Arc<dyn BudgetEnforcer> = Arc::new(LocalBudgetEnforcer::new(obs));
    LocalSessionManager::new(
        cs,
        mw,
        v,
        be,
        Observability::new(PathBuf::from("/dev/null"), false),
        0,
        tmp.path().join("sessions"),
    )
}

pub(crate) fn make_engine(
    tmp: &tempfile::TempDir,
    content_store: Arc<dyn ContentStore>,
    mw: Arc<dyn ManifestWriter>,
    dh: Arc<DeterminismHarness>,
    sm: Arc<LocalSessionManager>,
) -> LocalReplayEngine {
    LocalReplayEngine::new(
        content_store,
        mw,
        dh,
        Observability::new(tmp.path().join("replay.log"), false),
        sm,
        tmp.path().join("sessions"),
    )
}

/// Build a minimal recorded session: write Header + N ActionReceipt entries + SessionTerminal.
/// Returns session_id and the receipt bytes used.
pub(crate) fn build_recorded_session(
    mw: &dyn ManifestWriter,
    sessions_root: &std::path::Path,
    n_actions: u64,
    receipt_payload: &[u8],
) -> (SessionId, Vec<Vec<u8>>) {
    let id = SessionId(format!("01TEST{:020}", n_actions));
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();

    let mut receipts = Vec::new();
    for i in 0..n_actions {
        let receipt_json = serde_json::json!({
            "action_id": i,
            "dom_after_hash": sha256_hex(receipt_payload),
            "network_hash": sha256_hex(b"net"),
            "console_lines": i,
        });
        let receipt_bytes = serde_jcs::to_string(&receipt_json).unwrap().into_bytes();
        mw.append(
            id.clone(),
            ManifestEntry::ActionReceipt {
                action_id: i,
                emitted_at_ms: 1_000_000 + i * 100,
                receipt_canonical_bytes: receipt_bytes.clone(),
                prev_hash: String::new(),
            },
        )
        .unwrap();
        receipts.push(receipt_bytes);
    }

    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: n_actions,
            emitted_at_ms: 1_000_000 + n_actions * 100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    (id, receipts)
}

pub(crate) fn sha256_hex(input: &[u8]) -> String {
    let d = digest(&SHA256, input);
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract (action_id, emitted_at_ms, receipt_canonical_bytes) tuples from WAL.
/// These are the fields that replay() copies exactly.
pub(crate) fn extract_action_receipts(
    sessions_root: &std::path::Path,
    id: &SessionId,
) -> Vec<(u64, u64, Vec<u8>)> {
    let content = std::fs::read_to_string(sessions_root.join(&id.0).join("manifest.wal")).unwrap();
    let mut out = Vec::new();
    for line in content.lines() {
        if let Ok(ManifestEntry::ActionReceipt {
            action_id,
            emitted_at_ms,
            receipt_canonical_bytes,
            ..
        }) = serde_json::from_str::<ManifestEntry>(line)
        {
            out.push((action_id, emitted_at_ms, receipt_canonical_bytes));
        }
    }
    out
}

/// Build a recorded session whose Header records `determinism_enabled = false`
/// (the `--no-determinism` shape) + one ActionReceipt + a terminal. Mirrors
/// `build_recorded_session` but threads the determinism flag into the Header.
pub(crate) fn build_non_deterministic_session(
    mw: &dyn ManifestWriter,
    sessions_root: &std::path::Path,
) -> SessionId {
    let id = SessionId("01TESTNODETERMINISM00000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    // determinism_enabled = false → the replay-refuse marker.
    mw.open_manifest_with_started_at(id.clone(), None, Some(1_000_000), None, None, false)
        .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000_100,
            receipt_canonical_bytes: serde_jcs::to_string(&serde_json::json!({"action_id": 0}))
                .unwrap()
                .into_bytes(),
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_000_200,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();
    id
}

/// Shorthand: full engine stack on a fresh tmp dir.
pub(crate) fn make_refusal_stack(
    tmp: &tempfile::TempDir,
) -> (
    std::path::PathBuf,
    Arc<LocalManifestWriter>,
    LocalReplayEngine,
) {
    let obs = make_obs(tmp);
    let sessions_root = tmp.path().join("sessions");
    let mw = make_manifest_writer(tmp, obs.clone());
    let cs = make_content_store(tmp, obs.clone());
    let dh = make_harness(42, mw.clone() as Arc<dyn ManifestWriter>);
    let sm = make_session_manager(tmp, mw.clone() as Arc<dyn ManifestWriter>, dh.clone(), obs);
    let engine = make_engine(
        tmp,
        cs as Arc<dyn ContentStore>,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh,
        sm,
    );
    (sessions_root, mw, engine)
}
