//! Time-travel `inspect`.

use loom_core::content_store::ContentStore;
use loom_core::manifest_writer::ManifestWriter;
use std::sync::Arc;

use crate::common::*;

// ---- time-travel inspect ----

#[test]
fn test_inspect_at_action_5_returns_entries_0_to_5() {
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

    let (id, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        10,
        b"data",
    );

    let snap = engine
        .inspect(id.clone(), Some(5))
        .expect("inspect should succeed");

    let entries = snap["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 6, "at_action=5 → entries 0-5 = 6 entries");
    let last_id = entries.last().unwrap()["action_id"].as_u64().unwrap();
    assert_eq!(last_id, 5, "last entry must be action_id=5");
}

#[test]
fn test_inspect_does_not_mutate_manifest() {
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

    let (id, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        5,
        b"data",
    );

    let wal_before = std::fs::read(sessions_root.join(&id.0).join("manifest.wal")).unwrap();

    engine
        .inspect(id.clone(), Some(3))
        .expect("inspect should succeed");
    engine
        .inspect(id.clone(), Some(1))
        .expect("second inspect should succeed");

    let wal_after = std::fs::read(sessions_root.join(&id.0).join("manifest.wal")).unwrap();
    assert_eq!(
        wal_before, wal_after,
        "inspect must not mutate the manifest WAL"
    );
}
