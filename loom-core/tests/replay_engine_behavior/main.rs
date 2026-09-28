// Behavior tests for replay-engine.
//
// Coverage:
//   test_replay_produces_bit_equal_receipt_bytes
//   test_replay_100x_produces_identical_receipt_bytes
//   test_replay_aborts_on_missing_non_screenshot_blob
//   test_replay_proceeds_on_missing_screenshot_blob
//   test_replay_installs_tape_driven_determinism
//   test_diff_action_count_delta_positive
//   test_diff_action_count_delta_zero_no_extras
//   test_diff_field_level_diff_on_dom_hash_mismatch
//   test_diff_screenshot_excluded_from_differences_count
//   test_diff_screenshot_in_screenshot_diffs_with_flag
//   test_inspect_at_action_5_returns_entries_0_to_5
//   test_inspect_does_not_mutate_manifest
//   test_replay_throughput_structural_exceeds_5x
//   Validate: test_validate_passes_intact_chain_and_present_blobs
//   Validate: test_validate_fails_on_broken_hash_chain
//   Validate: test_validate_fails_on_missing_blob
//   Tape:     test_tape_persisted_and_loaded

mod common;
mod diff;
mod header;
mod inspect;
mod refusal;
mod replay;
mod validate;

use loom_core::content_store::ContentStore;
use loom_core::manifest_writer::ManifestWriter;
use loom_core::replay_engine::{ReplayEngine, ReplayOpts};
use std::sync::Arc;

use crate::common::*;

// ---- replay speed >= 5x real-time ----
//
// Kept at the crate root: `.config/nextest.toml` names this test in its
// timing-sensitive filter, and a module prefix would change its name.

#[test]
fn test_replay_throughput_structural_exceeds_5x() {
    // Structural replay is disk I/O only (no WASM execution).
    // Simulate a 60s real-time session with 600 actions (100ms each).
    // Replay should complete in <<12s (60/5). We assert it completes in <5s.
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
        600,
        b"perf-payload",
    );

    let start = std::time::Instant::now();
    engine
        .replay(id, ReplayOpts::default())
        .expect("replay should succeed");
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 5,
        "structural replay of 600 actions must complete in <5s, took {:?}",
        elapsed
    );
}
