//! Replay refusals: every refusal path emits its intended typed code and message.

use loom_core::content_store::ContentStore;
use loom_core::error::LoomErrorCode;
use loom_core::manifest_writer::{ManifestEntry, ManifestWriter, SessionId};
use loom_core::replay_engine::{ReplayEngine, ReplayOpts};
use std::sync::Arc;

use crate::common::*;

// ---- settle-capture (4b): replay refuses a non-deterministic session ----

#[test]
fn replay_refuses_non_deterministic_session() {
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

    let source_id =
        build_non_deterministic_session(mw.as_ref() as &dyn ManifestWriter, &sessions_root);

    let err = engine
        .replay(source_id.clone(), ReplayOpts::default())
        .expect_err("replay MUST refuse a --no-determinism session (it can never be replay-equal)");
    assert_eq!(
        err.code,
        loom_core::error::LoomErrorCode::NotReplayable,
        "refusal must be a typed NotReplayable (NOT InvalidArgument, which \
         degrades to schema_violation on the wire), got {err:?}"
    );
    assert!(
        err.message.contains("--no-determinism") && err.message.contains("NOT replayable"),
        "refusal message must carry the full compiled-in explanation; got: {}",
        err.message
    );
    assert!(
        err.message.contains(&source_id.0),
        "refusal message must name the offending session; got: {}",
        err.message
    );
}

// ---- replay-refusal fidelity audit: every refusal path must emit its ----
// ---- intended typed code + human message (no catch-all degradation). ----
// The wire-level counterpart (code + message surviving to the JSON-RPC
// envelope) is pinned by loom-rpc/tests/replay_refusal_wire.rs.

#[test]
fn replay_refuses_crashed_source_with_typed_session_aborted() {
    let tmp = tmp_path();
    let (sessions_root, mw, engine) = make_refusal_stack(&tmp);

    let id = SessionId("01TESTCRASHEDSOURCE000000".to_string());
    std::fs::create_dir_all(sessions_root.join(&id.0)).unwrap();
    mw.open_manifest(id.clone(), None).unwrap();
    mw.append(
        id.clone(),
        ManifestEntry::RuntimeCrash {
            last_completed_action_id: 0,
            emitted_at_ms: 1_000_100,
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let err = engine
        .replay(id.clone(), ReplayOpts::default())
        .expect_err("replay must refuse a crashed source");
    assert_eq!(err.code, LoomErrorCode::SessionAborted);
    assert_eq!(
        err.message,
        format!(
            "session {} crashed mid-flow; replay refuses to reproduce a partial trace",
            id.0
        ),
        "crashed-source refusal must carry its compiled-in explanation"
    );
}

#[test]
fn replay_refuses_aborted_source_with_typed_session_aborted() {
    let tmp = tmp_path();
    let (sessions_root, mw, engine) = make_refusal_stack(&tmp);

    let id = SessionId("01TESTABORTEDSOURCE000000".to_string());
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

    let err = engine
        .replay(id.clone(), ReplayOpts::default())
        .expect_err("replay must refuse an aborted source");
    assert_eq!(err.code, LoomErrorCode::SessionAborted);
    assert_eq!(
        err.message,
        format!(
            "session {} ended via abort (reason=user-initiated); \
             replay refuses to reproduce an abandoned trace",
            id.0
        ),
        "aborted-source refusal must carry the abort reason"
    );
}

#[test]
fn replay_refuses_broken_chain_with_typed_manifest_corrupt() {
    let tmp = tmp_path();
    let (sessions_root, mw, engine) = make_refusal_stack(&tmp);

    let (id, _) = build_recorded_session(mw.as_ref(), &sessions_root, 1, b"chain-tamper-payload");

    // Tamper: append a line whose prev_hash cannot match.
    let wal_path = sessions_root.join(&id.0).join("manifest.wal");
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .unwrap();
    writeln!(f, r#"{{"kind":"action_receipt","prev_hash":"bad_hash","action_id":99,"emitted_at_ms":9,"receipt_canonical_bytes":[]}}"#).unwrap();

    let err = engine
        .replay(id, ReplayOpts::default())
        .expect_err("replay must refuse a broken hash chain");
    assert_eq!(
        err.code,
        LoomErrorCode::ManifestCorrupt,
        "broken chain must surface as ManifestCorrupt (NOT a store/internal catch-all)"
    );
    assert!(
        err.message.contains("hash chain broken at index"),
        "chain refusal must name the break point; got: {}",
        err.message
    );
    assert!(
        err.context.is_some(),
        "chain refusal carries structured context (failed_at_index, hashes)"
    );
}
