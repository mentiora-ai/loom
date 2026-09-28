//! Replay itself: bit-equal receipts, missing-blob policy, tape determinism, session close.

use loom_core::content_store::ContentStore;
use loom_core::determinism_harness::{DeterminismHarness, SideEffectTape, TapeFrame};
use loom_core::error::LoomErrorCode;
use loom_core::manifest_writer::{LocalManifestWriter, ManifestEntry, ManifestWriter, SessionId};
use loom_core::observability::Observability;
use loom_core::replay_engine::{ReplayEngine, ReplayOpts};
use loom_core::session_manager::SessionStatus;
use std::sync::Arc;

use crate::common::*;

// ---- bit-equal receipt bytes ----

#[test]
fn test_replay_produces_bit_equal_receipt_bytes() {
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

    let (source_id, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        5,
        b"payload",
    );

    let replay_id = engine
        .replay(source_id.clone(), ReplayOpts::default())
        .expect("replay should succeed");

    // receipt_canonical_bytes must be byte-for-byte identical.
    // (emitted_at_ms is also copied from source to preserve hash-chain equality)
    let source_receipts = extract_action_receipts(&sessions_root, &source_id);
    let replay_receipts = extract_action_receipts(&sessions_root, &replay_id);
    assert_eq!(
        source_receipts.len(),
        replay_receipts.len(),
        "same number of actions"
    );
    for ((sa, se, sb), (ra, re, rb)) in source_receipts.iter().zip(replay_receipts.iter()) {
        assert_eq!(sa, ra, "action_id order preserved");
        assert_eq!(se, re, "emitted_at_ms copied from source");
        assert_eq!(
            sb, rb,
            "receipt bytes for action {sa} must be byte-identical"
        );
    }
}

// 100 replays all produce identical receipt bytes
#[test]
fn test_replay_100x_produces_identical_receipt_bytes() {
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

    let (source_id, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        3,
        b"deterministic",
    );
    let source_receipts = extract_action_receipts(&sessions_root, &source_id);

    for _ in 0..100 {
        let replay_id = engine
            .replay(source_id.clone(), ReplayOpts::default())
            .expect("replay should succeed");
        let replay_receipts = extract_action_receipts(&sessions_root, &replay_id);
        assert_eq!(
            source_receipts, replay_receipts,
            "each of 100 replays must have byte-identical receipt content"
        );
    }
}

// ---- replay refuses on missing non-screenshot blob ----

#[test]
fn test_replay_aborts_on_missing_non_screenshot_blob_with_correct_error() {
    let tmp = tmp_path();
    let obs = make_obs(&tmp);
    let sessions_root = tmp.path().join("sessions");
    let mw = make_manifest_writer(&tmp, obs.clone());
    // Use a real but EMPTY content store — all blob gets will return StoreNotFound.
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

    let id = SessionId("01TESTMISSBLOB0000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();

    let receipt_json = serde_json::json!({
        "action_id": 0,
        "dom_after_hash": "a".repeat(64),
        "content_refs": [{"sha256": "a".repeat(64), "size_bytes": 100, "kind": "dom"}]
    });
    let receipt_bytes = serde_jcs::to_string(&receipt_json).unwrap().into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000_000,
            receipt_canonical_bytes: receipt_bytes,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_000_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.replay(id, ReplayOpts::default());
    assert!(
        result.is_err(),
        "replay must abort when non-screenshot blob is missing"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err.code,
        LoomErrorCode::ReplayMissingBlob,
        "error code must be ReplayMissingBlob, got {:?}",
        err.code
    );
    // Refusal-fidelity audit: the message names the missing blob + kind so
    // the wire layer can pass it through verbatim.
    assert!(
        err.message.contains("pre-flight: missing blob"),
        "missing-blob refusal must carry the pre-flight explanation; got: {}",
        err.message
    );
}

// Screenshot blobs missing → replay proceeds (not abort)
#[test]
fn test_replay_proceeds_on_missing_screenshot_blob() {
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

    let id = SessionId("01TESTSCREENSHOT00000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();

    let receipt_json = serde_json::json!({
        "action_id": 0,
        "screenshot_hash": "b".repeat(64),
        "content_refs": [{"sha256": "b".repeat(64), "size_bytes": 200, "kind": "screenshot"}]
    });
    let receipt_bytes = serde_jcs::to_string(&receipt_json).unwrap().into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000_000,
            receipt_canonical_bytes: receipt_bytes,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_000_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.replay(id, ReplayOpts::default());
    assert!(
        result.is_ok(),
        "replay must NOT abort for missing screenshot blob: {:?}",
        result.err()
    );
}

// video-capture (THE determinism gate): a screencast `.webm` blob is
// non-deterministic + EXCLUDED from the replay integrity gate exactly like a
// screenshot. A receipt that references a `screencast_after_blob_ref` whose blob
// is absent from the CAS must still replay (proving the named-field blob
// collection tags it "screencast" and the pre-flight skips excluded kinds).
#[test]
fn test_replay_proceeds_on_missing_screencast_blob() {
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

    let id = SessionId("01TESTSCREENCAST00000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();

    // Real receipts carry the screencast blob in the NAMED field — exercise the
    // `collect_content_refs` path that tags it kind "screencast".
    let receipt_json = serde_json::json!({
        "action_id": 0,
        "screencast_after_hash": "c".repeat(64),
        "screencast_after_blob_ref": {"sha256": "c".repeat(64), "size_bytes": 4096},
    });
    let receipt_bytes = serde_jcs::to_string(&receipt_json).unwrap().into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000_000,
            receipt_canonical_bytes: receipt_bytes,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_000_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.replay(id, ReplayOpts::default());
    assert!(
        result.is_ok(),
        "replay must NOT abort for a missing screencast blob (excluded like screenshots): {:?}",
        result.err()
    );
}

// voice-call-io task 06 (AC7): captured-audio WAV bytes are non-deterministic +
// EXCLUDED from the replay integrity gate exactly like screencast. A receipt that
// references an `audio_after_blob_ref` whose blob is absent from the CAS must still
// replay — proving `collect_content_refs` tags it kind "audio" and the pre-flight
// skips excluded kinds.
#[test]
fn test_replay_proceeds_on_missing_audio_blob() {
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

    let id = SessionId("01TESTAUDIOCAP0000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();

    // Real stop_audio_capture receipts carry the WAV blob in the NAMED field —
    // exercise the `collect_content_refs` path that tags it kind "audio".
    let receipt_json = serde_json::json!({
        "action_id": 0,
        "audio_after_hash": "d".repeat(64),
        "audio_after_blob_ref": {"sha256": "d".repeat(64), "size_bytes": 32044},
    });
    let receipt_bytes = serde_jcs::to_string(&receipt_json).unwrap().into_bytes();
    mw.append(
        id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000_000,
            receipt_canonical_bytes: receipt_bytes,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_000_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let result = engine.replay(id, ReplayOpts::default());
    assert!(
        result.is_ok(),
        "replay must NOT abort for a missing audio blob (excluded like screencast): {:?}",
        result.err()
    );
}

// ---- tape-driven determinism installed ----

#[test]
fn test_replay_installs_tape_driven_determinism() {
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

    // Write a tape.jsonl for the source session containing a clock frame
    let source_id = SessionId("01TESTTAPEREP0000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&source_id.0)).unwrap();
    let tape_path = sessions_root.join(&source_id.0).join("tape.jsonl");
    let tape_line = serde_jcs::to_string(&TapeFrame::ClockRead { observed_ns: 9876 }).unwrap();
    std::fs::write(&tape_path, format!("{tape_line}\n")).unwrap();

    mw.open_manifest(source_id.clone(), None).unwrap();
    let receipt_bytes = serde_jcs::to_string(
        &serde_json::json!({"action_id": 0, "dom_after_hash": "c".repeat(64)}),
    )
    .unwrap()
    .into_bytes();
    mw.append(
        source_id.clone(),
        ManifestEntry::ActionReceipt {
            action_id: 0,
            emitted_at_ms: 1_000_000,
            receipt_canonical_bytes: receipt_bytes,
            prev_hash: String::new(),
        },
    )
    .unwrap();
    mw.append(
        source_id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 1,
            emitted_at_ms: 1_000_100,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let engine = make_engine(
        &tmp,
        cs.clone() as Arc<dyn ContentStore>,
        mw.clone() as Arc<dyn ManifestWriter>,
        dh.clone(),
        sm.clone(),
    );

    // Replay should succeed — tape was loaded and install_replay_mode called
    let result = engine.replay(source_id, ReplayOpts::default());
    assert!(
        result.is_ok(),
        "replay with tape should succeed: {:?}",
        result.err()
    );
}

// ---- Tape persistence ----

#[test]
fn test_tape_persisted_and_loaded() {
    let tmp = tmp_path();
    let sessions_root = tmp.path().join("sessions");
    let session_id = "01TESTTAPEIO00000000";
    std::fs::create_dir_all(sessions_root.join(session_id)).unwrap();

    let obs = Observability::new(tmp.path().join("loom.log"), false);
    let mw: Arc<dyn ManifestWriter> =
        Arc::new(LocalManifestWriter::new(tmp.path().join("sessions"), obs));
    let dh = DeterminismHarness::new(42, mw);
    let mut tw = dh.new_tape_writer();
    tw.record(TapeFrame::ClockRead { observed_ns: 12345 });
    tw.record(TapeFrame::RngDraw {
        value_u64: 0xdeadbeef,
    });
    tw.record(TapeFrame::ClockRead { observed_ns: 99999 });

    // Persist to disk
    tw.persist(&sessions_root, session_id)
        .expect("persist should succeed");

    // Load back
    let loaded =
        SideEffectTape::load_from_file(&sessions_root, session_id).expect("load should succeed");

    assert_eq!(loaded.frames.len(), 3, "should load 3 frames");
    match &loaded.frames[0] {
        TapeFrame::ClockRead { observed_ns } => assert_eq!(*observed_ns, 12345),
        _ => panic!("frame 0 should be ClockRead"),
    }
    match &loaded.frames[1] {
        TapeFrame::RngDraw { value_u64 } => assert_eq!(*value_u64, 0xdeadbeef),
        _ => panic!("frame 1 should be RngDraw"),
    }
}

// === replay closes its session coherently through the SessionManager ===

// Regression (audit 2026-06-10): replay() used to append the
// 'replay_complete' SessionTerminal directly via manifest_writer, leaving the
// in-memory session Active with last_activity_ms pinned to the SOURCE's
// original started_at_ms — instantly idle-reapable, so the reaper appended a
// SECOND SessionTerminal{idle_ttl} over the completed replay manifest.
#[test]
fn test_replay_closes_session_in_fsm_and_reaper_cannot_double_terminal() {
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

    let (source_id, _) = build_recorded_session(
        mw.as_ref() as &dyn ManifestWriter,
        &sessions_root,
        2,
        b"fsm-close",
    );

    let replay_id = engine
        .replay(source_id, ReplayOpts::default())
        .expect("replay should succeed");

    // The in-memory FSM must be terminal, not Active.
    let session = sm
        .get(replay_id.clone())
        .expect("replay session retained in-memory (bounded terminal retention)");
    assert_eq!(
        *session.status.lock(),
        SessionStatus::Closed,
        "replay() must close its session through the SessionManager FSM"
    );

    // Simulate the idle reaper hitting the session with an ancient
    // last_activity clock: the two-phase guard must SPARE it (not Active),
    // never appending a second terminal.
    let far_future = 4_102_444_800_000u64; // 2100-01-01 — any 'now' past the source epoch
    let evicted = sm
        .evict_if_idle(replay_id.clone(), 1, far_future)
        .expect("evict_if_idle on a closed session is not an error for the sweep");
    assert!(
        !evicted,
        "idle reaper must spare the already-closed replay session"
    );

    // Exactly ONE SessionTerminal, with reason 'replay_complete'.
    let content =
        std::fs::read_to_string(sessions_root.join(&replay_id.0).join("manifest.wal")).unwrap();
    let terminals: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"] == "session_terminal")
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "replay manifest must contain exactly one SessionTerminal"
    );
    assert_eq!(
        terminals[0]["reason"], "replay_complete",
        "the single terminal must be the replay_complete one"
    );

    // Replay-of-replay stays allowed (the 1b abort-guard accepts
    // reason=replay_complete; an idle_ttl double-terminal would refuse it).
    engine
        .replay(replay_id, ReplayOpts::default())
        .expect("replaying a cleanly completed replay must stay allowed");
}
