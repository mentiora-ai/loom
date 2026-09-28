//! The replay session's manifest Header: started_at_ms, budgets, capture policy.

use loom_core::budget_enforcer::BudgetLimits;
use loom_core::content_store::ContentStore;
use loom_core::manifest_writer::{ManifestEntry, ManifestWriter, SessionId};
use loom_core::replay_engine::{ReplayEngine, ReplayOpts};
use std::sync::Arc;

use crate::common::*;

// ---- started_at_ms is propagated from source to replay ----
//
// Verifies that the replay session's manifest Header carries the source
// session's `started_at_ms` (not `now_ms()` at replay time). This is the
// foundation for hash-chain bit-equality: the chain
// hashes over the canonical Header bytes, so any divergence in the
// Header poisons every subsequent prev_hash.

#[test]
fn test_replay_header_started_at_ms_matches_source() {
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
        sm,
    );

    // Build a small recorded session.
    let (source_id, _receipts) =
        build_recorded_session(mw.as_ref(), &sessions_root, 1, b"ac-shcrt-08-payload");

    // Read the source's started_at_ms from its Header.
    let source_started_at_ms = read_header_started_at_ms(&sessions_root, &source_id)
        .expect("source session must have a Header entry");

    // Replay.
    let replay_id = engine
        .replay(source_id.clone(), ReplayOpts::default())
        .expect("replay must succeed");

    // The replay session's Header must carry the source's started_at_ms.
    let replay_started_at_ms = read_header_started_at_ms(&sessions_root, &replay_id)
        .expect("replay session must have a Header entry");

    assert_eq!(
        source_started_at_ms, replay_started_at_ms,
        "replay Header started_at_ms must equal source's \
         (chain bit-equality requires deterministic Header bytes)"
    );

    // The action_receipt's prev_hash MUST also match the source — the
    // chain hashes over the Header's canonical bytes, and with
    // started_at_ms now equal the only Header field that differs is
    // session_id. Confirm at least the receipt content is bit-equal
    // (this is the existing bit-equal-receipt guarantee).
    let source_receipts = extract_action_receipts(&sessions_root, &source_id);
    let replay_receipts = extract_action_receipts(&sessions_root, &replay_id);
    assert_eq!(
        source_receipts, replay_receipts,
        "replay action_receipts must be bit-equal to source"
    );
}

/// Helper: read the `started_at_ms` field from the Header entry (first
/// line of `manifest.wal`).
fn read_header_started_at_ms(sessions_root: &std::path::Path, id: &SessionId) -> Option<u64> {
    let path = sessions_root.join(&id.0).join("manifest.wal");
    let content = std::fs::read_to_string(&path).ok()?;
    let first = content.lines().next()?;
    if let Ok(ManifestEntry::Header { started_at_ms, .. }) =
        serde_json::from_str::<ManifestEntry>(first)
    {
        Some(started_at_ms)
    } else {
        None
    }
}

#[test]
fn two_consecutive_replays_produce_identical_headers() {
    // Replay determinism: replaying the same source twice produces two
    // sessions with identical Header started_at_ms values (both equal
    // to the source). This is the meaningful "deterministic chain"
    // guarantee given that session_id intentionally differs per session.
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
        sm,
    );

    let (source_id, _) =
        build_recorded_session(mw.as_ref(), &sessions_root, 1, b"two-replays-payload");
    let source_ts = read_header_started_at_ms(&sessions_root, &source_id).unwrap();

    let r1 = engine
        .replay(source_id.clone(), ReplayOpts::default())
        .unwrap();
    let r2 = engine
        .replay(source_id.clone(), ReplayOpts::default())
        .unwrap();

    let r1_ts = read_header_started_at_ms(&sessions_root, &r1).unwrap();
    let r2_ts = read_header_started_at_ms(&sessions_root, &r2).unwrap();

    assert_eq!(
        source_ts, r1_ts,
        "replay 1 Header started_at_ms must equal source"
    );
    assert_eq!(
        source_ts, r2_ts,
        "replay 2 Header started_at_ms must equal source"
    );
    assert_eq!(
        r1_ts, r2_ts,
        "two consecutive replays must produce identical Header timestamps"
    );
}

// === replay Header fidelity: budgets + capture_policy (audit 2026-06-10) ===

/// Read every WAL line's `prev_hash` field as a string ("" for the Header's
/// null). The prev_hash chain seeds from the projected Header bytes, so two
/// manifests with equal vectors have bit-equal chains at every line index.
fn read_prev_hashes(sessions_root: &std::path::Path, id: &SessionId) -> Vec<String> {
    let content = std::fs::read_to_string(sessions_root.join(&id.0).join("manifest.wal")).unwrap();
    content
        .lines()
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            v.get("prev_hash")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// First WAL line (the Header) parsed as JSON.
fn read_header_json(sessions_root: &std::path::Path, id: &SessionId) -> serde_json::Value {
    let content = std::fs::read_to_string(sessions_root.join(&id.0).join("manifest.wal")).unwrap();
    serde_json::from_str(content.lines().next().expect("WAL has a Header line")).unwrap()
}

// Regression: a source recorded with --budget/--capture-policy must replay
// with the SAME Header budgets/capture_policy. Both fields serialize with
// `skip_serializing_if`, and hashable_line() only projects out
// session_id/started_at_ms/emitted_at_ms — so dropping them (the pre-fix
// `limits: None, capture_policy: None`) changed the projected Header hash and
// poisoned every subsequent prev_hash in the replay chain.
#[test]
fn test_replay_header_preserves_budgets_and_capture_policy_chain_bit_equal() {
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

    // Source recorded with explicit budgets + capture policy (+ seed, so the
    // replay Header round-trips every skip-if-none field).
    let limits = BudgetLimits {
        session_walltime_ms: 120_000,
        action_walltime_ms: 9_000,
        network_bytes: 1_000_000,
        dom_nodes: 7_000,
        js_heap_bytes: 64 * 1024 * 1024,
    };
    let source_id = SessionId("01TESTHEADERFIDELITY0000AB".to_string());
    std::fs::create_dir_all(sessions_root.join(&source_id.0)).unwrap();
    mw.open_manifest_with_started_at(
        source_id.clone(),
        Some(limits),
        None,
        Some("minimal".to_string()),
        Some(7),
        true,
    )
    .unwrap();
    for i in 0..2u64 {
        mw.append(
            source_id.clone(),
            ManifestEntry::ActionReceipt {
                action_id: i,
                emitted_at_ms: 1_000_000 + i * 100,
                receipt_canonical_bytes: serde_jcs::to_string(&serde_json::json!({"action_id": i}))
                    .unwrap()
                    .into_bytes(),
                prev_hash: String::new(),
            },
        )
        .unwrap();
    }
    mw.append(
        source_id.clone(),
        ManifestEntry::SessionTerminal {
            action_id: 2,
            emitted_at_ms: 1_000_300,
            reason: "close".to_string(),
            prev_hash: String::new(),
        },
    )
    .unwrap();

    let replay_id = engine
        .replay(source_id.clone(), ReplayOpts::default())
        .expect("replay of a budget/capture-policy session should succeed");

    // The replay Header carries the source's recorded budgets + capture_policy.
    let source_header = read_header_json(&sessions_root, &source_id);
    let replay_header = read_header_json(&sessions_root, &replay_id);
    assert_eq!(
        replay_header.get("budgets"),
        source_header.get("budgets"),
        "replay Header must reproduce the source's recorded budgets"
    );
    assert!(
        source_header.get("budgets").is_some(),
        "precondition: source Header actually recorded budgets"
    );
    assert_eq!(
        replay_header.get("capture_policy"),
        source_header.get("capture_policy"),
        "replay Header must reproduce the source's capture_policy"
    );
    assert_eq!(
        source_header.get("capture_policy").and_then(|v| v.as_str()),
        Some("minimal"),
        "precondition: source Header actually recorded capture_policy"
    );

    // Header fidelity modulo the two projected ephemerals: stripping
    // session_id + started_at_ms, the Headers must be IDENTICAL (JCS sorts
    // keys, so Value equality == canonical-byte equality).
    let strip = |mut v: serde_json::Value| {
        let obj = v.as_object_mut().unwrap();
        obj.remove("session_id");
        obj.remove("started_at_ms");
        v
    };
    assert_eq!(
        strip(source_header),
        strip(replay_header),
        "replay Header must match the source Header on every non-ephemeral field"
    );

    // Chain bit-equality: the prev_hash at EVERY line index must match the
    // source's (index 1 is sha256 of the projected Header — the chain seed).
    assert_eq!(
        read_prev_hashes(&sessions_root, &source_id),
        read_prev_hashes(&sessions_root, &replay_id),
        "replay prev_hash chain must be bit-equal to the source chain at every index"
    );

    mw.validate(replay_id)
        .expect("replay manifest hash chain must validate");
}
