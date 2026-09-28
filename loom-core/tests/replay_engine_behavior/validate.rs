//! `validate`: chain and blob integrity, and PASS vs replayable.

use loom_core::content_store::ContentStore;
use loom_core::manifest_writer::{ManifestEntry, ManifestWriter, SessionId};
use std::sync::Arc;

use crate::common::*;

// ---- Validate tests ----

#[test]
fn test_validate_passes_intact_chain_and_present_blobs() {
    let tmp = tmp_path();
    let obs = make_obs(&tmp);
    let sessions_root = tmp.path().join("sessions");
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone());
    let dh = make_harness(42, mw.clone() as Arc<dyn ManifestWriter>);
    let sm = make_session_manager(
        &tmp,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        obs.clone(),
    );
    let engine = make_engine(
        &tmp,
        cs.clone() as Arc<dyn ContentStore>,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        sm.clone(),
    );

    // Store a blob in CAS
    let blob = b"important content";
    let cr = cs.put(blob).unwrap();

    let id = SessionId("01TESTVALIDOKK000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();
    let receipt = serde_jcs::to_string(&serde_json::json!({
        "action_id": 0,
        "dom_after_hash": cr.sha256,
        "content_refs": [{"sha256": cr.sha256, "size_bytes": cr.size_bytes, "kind": "dom"}]
    }))
    .unwrap()
    .into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: receipt,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.validate(id).expect("validate should succeed");
    assert!(
        result.passed,
        "intact chain + present blobs → validate passes"
    );
    assert!(
        result.reasons.is_empty(),
        "no reasons on pass: {:?}",
        result.reasons
    );
}

#[test]
fn test_validate_fails_on_broken_hash_chain() {
    let tmp = tmp_path();
    let obs = make_obs(&tmp);
    let sessions_root = tmp.path().join("sessions");
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone());
    let dh = make_harness(42, mw.clone() as Arc<dyn ManifestWriter>);
    let sm = make_session_manager(
        &tmp,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        obs.clone(),
    );
    let engine = make_engine(
        &tmp,
        cs.clone() as Arc<dyn ContentStore>,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        sm.clone(),
    );

    let id = SessionId("01TESTBROKENHASH0000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();
    let receipt = serde_jcs::to_string(&serde_json::json!({"action_id": 0}))
        .unwrap()
        .into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: receipt,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    // Corrupt the WAL by appending a line with a bad prev_hash
    let wal_path = sessions_root.join(&id.0).join("manifest.wal");
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .unwrap();
    writeln!(f, r#"{{"kind":"action_receipt","prev_hash":"bad_hash","action_id":99,"emitted_at_ms":9,"receipt_canonical_bytes":[]}}"#).unwrap();

    let result = engine.validate(id).expect("validate call should not panic");
    assert!(!result.passed, "broken hash chain → validate must fail");
    assert!(
        !result.reasons.is_empty(),
        "must provide reason for failure"
    );
}

#[test]
fn test_validate_fails_on_missing_blob() {
    let tmp = tmp_path();
    let obs = make_obs(&tmp);
    let sessions_root = tmp.path().join("sessions");
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone()); // empty CAS
    let dh = make_harness(42, mw.clone() as Arc<dyn ManifestWriter>);
    let sm = make_session_manager(
        &tmp,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        obs.clone(),
    );
    let engine = make_engine(
        &tmp,
        cs.clone() as Arc<dyn ContentStore>,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        sm.clone(),
    );

    let id = SessionId("01TESTVALIDMISSB0000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();
    let receipt = serde_jcs::to_string(&serde_json::json!({
        "action_id": 0,
        "dom_after_hash": "j".repeat(64),
        "content_refs": [{"sha256": "j".repeat(64), "size_bytes": 100, "kind": "dom"}]
    }))
    .unwrap()
    .into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: receipt,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.validate(id).expect("validate call should not panic");
    assert!(!result.passed, "missing blob → validate must fail");
    let reason_contains_blob = result
        .reasons
        .iter()
        .any(|r| r.contains("missing") || r.contains("blob") || r.contains("StoreNotFound"));
    assert!(
        reason_contains_blob,
        "reason must mention missing blob, got: {:?}",
        result.reasons
    );
}

// (audit 2026-06-10, "Blob-presence validation and replay pre-flight inspect a
// 'content_refs' field that no production receipt contains"):
// `collect_content_refs` now walks the real `ReceiptPayload` blob-ref fields
// (`dom_after_blob_ref`, `dom_before_blob_ref`, `return_value_blob_ref`,
// `screenshot_*_blob_ref`, `network_events[].response_body_ref`), so
// `validate()` and the `ReplayMissingBlob` pre-flight detect missing CAS blobs
// for real recordings — not just the phantom `content_refs` array. This test
// pins that a missing `dom_after_blob_ref` blob fails validation. FIXED.
#[test]
fn validate_must_fail_on_missing_production_shape_blob_ref() {
    let tmp = tmp_path();
    let obs = make_obs(&tmp);
    let sessions_root = tmp.path().join("sessions");
    let mw = make_manifest_writer(&tmp, obs.clone());
    let cs = make_content_store(&tmp, obs.clone()); // empty CAS — nothing present
    let dh = make_harness(42, mw.clone() as Arc<dyn ManifestWriter>);
    let sm = make_session_manager(
        &tmp,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        obs.clone(),
    );
    let engine = make_engine(
        &tmp,
        cs.clone() as Arc<dyn ContentStore>,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        sm.clone(),
    );

    let id = SessionId("01TESTVALIDPRODREF00".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();
    // PRODUCTION receipt shape: named blob-ref fields, NO `content_refs`.
    // This is what `ReceiptMarshaller` / `ReceiptPayload` actually emit.
    let receipt = serde_jcs::to_string(&serde_json::json!({
        "action_id": 0,
        "dom_after_hash": "j".repeat(64),
        "dom_after_blob_ref": {"sha256": "j".repeat(64), "size_bytes": 100}
    }))
    .unwrap()
    .into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: receipt,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.validate(id).expect("validate call should not panic");
    assert!(
        !result.passed,
        "a receipt whose dom_after_blob_ref points at a blob absent from the \
         CAS must fail validation — production receipts never carry the \
         legacy `content_refs` array this check currently looks for"
    );
}

// ---- ValidationResult.replayable: PASS ≠ replayable ----

#[test]
fn validate_reports_no_determinism_session_as_pass_but_not_replayable() {
    let tmp = tmp_path();
    let (sessions_root, mw, engine) = make_refusal_stack(&tmp);

    let id = build_non_deterministic_session(mw.as_ref() as &dyn ManifestWriter, &sessions_root);

    let result = engine
        .validate(id.clone())
        .expect("validate must not error");
    assert!(
        result.passed,
        "a --no-determinism session has an intact chain → integrity PASSes"
    );
    assert!(
        !result.replayable,
        "PASS must not imply replayable for a --no-determinism session"
    );
    let reason = result.not_replayable_reason.expect("reason must be set");
    assert!(
        reason.contains("--no-determinism") && reason.contains(&id.0),
        "reason must be the replay refusal explanation; got: {reason}"
    );
}

#[test]
fn validate_reports_clean_deterministic_session_as_replayable() {
    let tmp = tmp_path();
    let (sessions_root, mw, engine) = make_refusal_stack(&tmp);

    // No content_refs → no blob requirements; clean close terminal.
    let (id, _) = build_recorded_session(mw.as_ref(), &sessions_root, 1, b"replayable-payload");

    let result = engine.validate(id).expect("validate must not error");
    assert!(
        result.passed,
        "clean session must PASS: {:?}",
        result.reasons
    );
    assert!(
        result.replayable,
        "clean deterministic session is replayable"
    );
    assert!(result.not_replayable_reason.is_none());
}

#[test]
fn validate_reports_aborted_session_as_not_replayable() {
    let tmp = tmp_path();
    let (sessions_root, mw, engine) = make_refusal_stack(&tmp);

    let id = SessionId("01TESTVALIDATEABORTED0000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 0,
            emitted_at_ms: 1_000_100,
            reason: "user-initiated".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.validate(id).expect("validate must not error");
    assert!(result.passed, "intact chain → integrity PASSes");
    assert!(
        !result.replayable,
        "aborted source is refused by replay → not replayable"
    );
    assert!(
        result
            .not_replayable_reason
            .expect("reason must be set")
            .contains("ended via abort"),
        "reason mirrors replay's abort refusal"
    );
}
