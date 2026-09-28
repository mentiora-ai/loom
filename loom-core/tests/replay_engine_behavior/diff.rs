//! `diff`: action-count deltas, per-receipt field diffs, screenshot exclusion.

use loom_core::content_store::ContentStore;
use loom_core::manifest_writer::{ManifestEntry, ManifestWriter, SessionId};
use loom_core::replay_engine::{DiffOpts, ReplayEngine};
use std::sync::Arc;

use crate::common::*;

// ---- diff action count delta ----

#[test]
fn test_diff_action_count_delta_positive() {
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

    let (id_a, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        3,
        b"payload",
    );
    let (id_b, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        4,
        b"payload",
    );

    let report = engine
        .diff(
            id_a.clone(),
            id_b.clone(),
            DiffOpts {
                exclude_screenshots: true,
                include_audit_entries: false,
            },
        )
        .expect("diff should succeed");

    assert_eq!(
        report.action_count_delta, 1,
        "B has one more action: delta should be +1"
    );
    assert_eq!(report.a.0, id_a.0);
    assert_eq!(report.b.0, id_b.0);
}

#[test]
fn test_diff_action_count_delta_zero_no_extras() {
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

    let (id_a, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        3,
        b"payload",
    );
    let (id_b, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        3,
        b"payload",
    );

    let report = engine
        .diff(
            id_a,
            id_b,
            DiffOpts {
                exclude_screenshots: true,
                include_audit_entries: false,
            },
        )
        .expect("diff should succeed");

    assert_eq!(
        report.action_count_delta, 0,
        "identical action counts → delta 0"
    );
    assert!(
        report.field_diffs.is_empty(),
        "identical receipts → no field diffs"
    );
}

// ---- per-receipt field diff ----

#[test]
fn test_diff_field_level_diff_on_dom_hash_mismatch() {
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

    // A: action 0 receipt has dom_after_hash of "version-a"
    let (id_a, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        1,
        b"version-a",
    );
    // B: action 0 receipt has dom_after_hash of "version-b" (different hash)
    let (id_b, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        1,
        b"version-b",
    );

    let report = engine
        .diff(
            id_a,
            id_b,
            DiffOpts {
                exclude_screenshots: true,
                include_audit_entries: false,
            },
        )
        .expect("diff should succeed");

    assert!(
        !report.field_diffs.is_empty(),
        "different dom_after_hash should produce field diff"
    );
    let diff = &report.field_diffs[0];
    assert_eq!(diff.action_id, 0);
    assert!(
        diff.field_path.contains("dom_after_hash"),
        "field_path should reference dom_after_hash, got: {}",
        diff.field_path
    );
}

// ---- screenshots excluded from differences ----

#[test]
fn test_diff_screenshot_excluded_from_differences_count() {
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

    // Session A: screenshot_hash = "eee..."
    let id_a = SessionId("01TESTSCRDIFFA000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id_a.0)).unwrap();
    mw.open_manifest(id_a.clone(), None).unwrap();
    let receipt_a = serde_jcs::to_string(&serde_json::json!({
        "action_id": 0, "dom_after_hash": "d".repeat(64), "screenshot_hash": "e".repeat(64)
    }))
    .unwrap()
    .into_bytes();
    mw.append(
        id_a.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: receipt_a,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id_a.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    // Session B: same dom_after_hash, different screenshot_hash
    let id_b = SessionId("01TESTSCRDIFFB000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id_b.0)).unwrap();
    mw.open_manifest(id_b.clone(), None).unwrap();
    let receipt_b = serde_jcs::to_string(&serde_json::json!({
        "action_id": 0, "dom_after_hash": "d".repeat(64), "screenshot_hash": "f".repeat(64)
    }))
    .unwrap()
    .into_bytes();
    mw.append(
        id_b.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: receipt_b,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id_b.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let report = engine
        .diff(
            id_a,
            id_b,
            DiffOpts {
                exclude_screenshots: true,
                include_audit_entries: false,
            },
        )
        .expect("diff should succeed");

    assert!(
        report.field_diffs.is_empty(),
        "screenshot-only diff → field_diffs must be empty (differences=0)"
    );
    assert_eq!(
        report.screenshot_diffs.len(),
        1,
        "screenshot hash mismatch should be in screenshot_diffs"
    );
}

#[test]
fn test_diff_screenshot_in_screenshot_diffs_with_flag() {
    // When exclude_screenshots=false, screenshot diffs appear in screenshot_diffs[] only,
    // NOT in field_diffs[].
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

    let id_a = SessionId("01TESTSCRINCLUDA0000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id_a.0)).unwrap();
    mw.open_manifest(id_a.clone(), None).unwrap();
    let ra = serde_jcs::to_string(&serde_json::json!({"action_id":0,"dom_after_hash":"g".repeat(64),"screenshot_hash":"h".repeat(64)}))
        .unwrap()
        .into_bytes();
    mw.append(
        id_a.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: ra,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id_a.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let id_b = SessionId("01TESTSCRINCLUB00000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id_b.0)).unwrap();
    mw.open_manifest(id_b.clone(), None).unwrap();
    let rb = serde_jcs::to_string(&serde_json::json!({"action_id":0,"dom_after_hash":"g".repeat(64),"screenshot_hash":"i".repeat(64)}))
        .unwrap()
        .into_bytes();
    mw.append(
        id_b.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000,
            receipt_canonical_bytes: rb,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id_b.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    // exclude_screenshots: false → screenshot diffs go in screenshot_diffs[], never field_diffs[]
    let report = engine
        .diff(
            id_a,
            id_b,
            DiffOpts {
                exclude_screenshots: false,
                include_audit_entries: false,
            },
        )
        .expect("diff ok");
    assert!(
        report.field_diffs.is_empty(),
        "screenshot diffs must NOT be in field_diffs"
    );
    assert!(
        !report.screenshot_diffs.is_empty(),
        "screenshot diffs should be in screenshot_diffs"
    );
    assert_eq!(report.action_count_delta, 0);
}
