//! Screencast recording through the shim.

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use std::time::Duration;

use crate::common::*;

/// Register a real shim wired to fake-chromium that emits `frames` synthetic
/// screencast frames on `Page.startScreencast`. Shared by the video-capture
/// e2e tests below. Panics with a build hint if the binaries are missing.
fn register_recording_shim(
    mgr: &ShimManager,
    id: &ShimId,
    user_data_dir: &std::path::Path,
    frames: u32,
) {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!("fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first");
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_path),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES".into(),
                    frames.to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );
}

/// e2e (video-capture): a real shim + fake-chromium screencast round-trip end to
/// end. `start_recording` → fake emits 3 valid-JPEG `Page.screencastFrame`s →
/// the shim's ScreencastRecorder buffers + acks each → `stop_recording` encodes
/// them with a REAL ffmpeg. Asserts the full protocol/streaming/ack path
/// (`frame_count`) AND, when ffmpeg is available, that the produced `.webm`
/// has the EBML magic AND is actually DECODABLE by ffmpeg (not just well-framed).
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn screencast_record_round_trip() {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-rec".into());
    register_recording_shim(&mgr, &id, user_data_dir.path(), 3);

    mgr.send_start_recording(id.clone(), 0, 0, 300_000, 268_435_456, 10)
        .await
        .expect("start_recording errored");
    // Let the fake's 3 frames stream in + get acked/buffered before stopping.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(180),
        mgr.send_stop_recording(id.clone(), 0, 0),
    )
    .await
    .expect("stop_recording did not return in 180s")
    .expect("stop_recording errored");

    // Core assertion (encoder-independent): the full start→frames→ack→stop
    // protocol path streamed + buffered all 3 frames.
    assert_eq!(outcome.frame_count, 3, "all 3 synthetic frames buffered");

    // stop_reason reports WHY recording stopped and is independent of whether the
    // encode succeeded: a normal stop with no cap hit is always "explicit", even
    // when a flaky ffmpeg download makes the encode fail. (This is the assertion
    // that used to flake — it now holds regardless of encoder availability.)
    assert_eq!(outcome.stop_reason, "explicit");
    assert_ne!(outcome.stop_reason, "encoder_unavailable");

    if outcome.error.is_none() {
        assert!(
            outcome.webm_bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
            "encoded bytes must start with the EBML/webm magic"
        );
        // Stronger than magic-bytes: the .webm must actually DECODE. Write it
        // out and have ffmpeg demux/decode it to null — a corrupt container or
        // bad stream would make ffmpeg exit non-zero. (CI installs ffmpeg, so
        // this real-encode branch is genuinely exercised there.)
        let webm_path = user_data_dir.path().join("out.webm");
        std::fs::write(&webm_path, &outcome.webm_bytes).expect("write webm");
        let probe = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&webm_path)
            .args(["-f", "null", "-"])
            .output();
        if let Ok(p) = probe {
            assert!(
                p.status.success(),
                "produced .webm did not decode: {}",
                String::from_utf8_lossy(&p.stderr)
            );
        }
    } else {
        // Best-effort encode failure (e.g. ffmpeg unavailable): no blob, and the
        // failure is described in `error` — assert its CONTENT, not just presence.
        assert!(outcome.webm_bytes.is_empty());
        let err = outcome.error.as_deref().unwrap_or_default().to_lowercase();
        assert!(
            err.contains("ffmpeg") || err.contains("encod"),
            "encode-failure error should mention ffmpeg/encode, got: {:?}",
            outcome.error
        );
    }

    mgr.shutdown_session("test-session-rec").await;
    drop(user_data_dir);
}

/// e2e (video-capture): stopping a recording that captured ZERO frames returns
/// the best-effort `no_frames` contract (no blob, error set) — exercised through
/// the real shim, not just the in-process recorder unit test.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn screencast_zero_frames_reports_no_frames_e2e() {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-rec0".into());
    register_recording_shim(&mgr, &id, user_data_dir.path(), 0); // emit no frames

    mgr.send_start_recording(id.clone(), 0, 0, 300_000, 268_435_456, 10)
        .await
        .expect("start_recording errored");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let outcome = mgr
        .send_stop_recording(id.clone(), 0, 0)
        .await
        .expect("stop_recording errored");

    assert_eq!(outcome.frame_count, 0);
    assert_eq!(outcome.stop_reason, "no_frames");
    assert!(outcome.error.is_some());
    assert!(outcome.webm_bytes.is_empty());

    mgr.shutdown_session("test-session-rec0").await;
    drop(user_data_dir);
}

/// e2e (video-capture): the byte cap drops over-cap frames through the real shim.
/// A cap that admits ~1 frame leaves `frame_count == 1` with `stop_reason ==
/// "byte_cap"` — proving the cap is enforced shim-side, not just in the unit test.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn screencast_byte_cap_enforced_e2e() {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-reccap".into());
    register_recording_shim(&mgr, &id, user_data_dir.path(), 5); // 5 frames offered

    // Each decoded JPEG is ~222 bytes; a 300-byte cap admits exactly one.
    mgr.send_start_recording(id.clone(), 0, 0, 0, 300, 10)
        .await
        .expect("start_recording errored");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(180),
        mgr.send_stop_recording(id.clone(), 0, 0),
    )
    .await
    .expect("stop_recording did not return")
    .expect("stop_recording errored");

    // Both assertions are encoder-INDEPENDENT: the byte cap is enforced shim-side
    // before the encode, so the cap-hit is knowable whether or not ffmpeg is
    // available. This is why the test no longer flakes when ffmpeg's runtime
    // download fails — an encode failure surfaces in `error`, never by overwriting
    // the stop reason.
    assert_eq!(outcome.frame_count, 1, "byte cap admits exactly one frame");
    assert_eq!(outcome.stop_reason, "byte_cap");
    assert_ne!(outcome.stop_reason, "encoder_unavailable");

    mgr.shutdown_session("test-session-reccap").await;
    drop(user_data_dir);
}
