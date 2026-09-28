//! voice-call-io task 07: the audio harness e2e (AC4 round-trip, AC10 missing-mic).

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use std::time::Duration;

use crate::common::*;

// ─── voice-call-io task 07: audio harness e2e (AC4 round-trip, AC10 missing-mic) ───
//
// These drive the FULL host→shim→fake-chromium audio path for the first time:
// `send_navigate(audio_enabled:true)` lazy-spawns the `--audio` target (installs the
// mic bootstrap, mints the nonce, grants `audioCapture`), then the typed audio senders
// exercise inject + start/stop capture. The `fake-chromium` audio harness answers the
// nonce'd in-page API calls with env-scripted synthetic data (echo / tone / no-gum).
//
// The fake stays DUMB (Architecture A17): echo replays the injected bytes verbatim, so
// these tests validate the WIRE-PLUMBING integrity (inject→enqueue→capture→drain→decode
// →resample→i16→WAV→CAS lost/reordered nothing), NOT real browser audio fidelity — that
// is owned by `resample.rs`/`wav.rs` unit tests and the `#[ignore]`d real-Chrome
// `loom-cli/tests/live_voice_e2e.rs`.

/// Register a shim wired to `fake-chromium` with arbitrary extra audio env
/// (`LOOM_FAKE_CHROMIUM_AUDIO_*`). Mirrors `register_recording_shim`.
fn register_audio_shim(
    mgr: &ShimManager,
    id: &ShimId,
    user_data_dir: &std::path::Path,
    extra_env: &[(&str, &str)],
) {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!("fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first");
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let mut env = vec![
        ("LOOM_SHIM_CHROMIUM_PATH".to_string(), fake_path),
        (
            "LOOM_SHIM_USER_DATA_DIR".to_string(),
            user_data_dir.display().to_string(),
        ),
        (
            "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".to_string(),
            user_data_dir.display().to_string(),
        ),
    ];
    env.extend(
        extra_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env,
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );
}

/// Navigate an `--audio` session's lazy-spawned target (installs the mic bootstrap +
/// mints the nonce + grants audioCapture). `determinism_enabled:false` is mandatory for
/// audio (PRD D5) and lets the default settle script settle immediately.
async fn navigate_audio_session(mgr: &ShimManager, id: &ShimId) {
    use loom_host::shim_manager::SendNavigateParams;
    use loom_shared::types::{EpochMs, Seed};
    let params = SendNavigateParams {
        id: id.clone(),
        action_id: String::new(),
        session_id: 0,
        target_id: 0,
        url: "https://call.example/".to_string(),
        budget_ms: 10_000,
        seed: Seed(0),
        epoch_ms: EpochMs(0),
        blocklist_enabled: false,
        until: "settled".to_string(),
        determinism_enabled: false,
        audio_enabled: true,
    };
    tokio::time::timeout(Duration::from_secs(30), mgr.send_navigate(params))
        .await
        .expect("send_navigate did not settle within 30s (audio session)")
        .expect("send_navigate errored");
}

/// Normalized f32 → i16, matching `audio_bridge::wav::f32_to_i16` (scale by 32767, round,
/// saturate). Trivial (3 lines) — the nontrivial resampler is NOT copied (D1).
fn f32_to_i16_ref(s: f32) -> i16 {
    if s.is_nan() {
        return 0;
    }
    (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// `rate*ms/1000` samples of `amp*sin(2π·freq·t)`.
fn sine(freq: f64, amp: f64, rate: u32, ms: u64) -> Vec<f32> {
    let n = (u64::from(rate) * ms / 1000) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            (amp * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32
        })
        .collect()
}

fn f32_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Parse the i16 sample body out of a canonical 44-byte-header mono WAV.
fn wav_body_i16(wav: &[u8]) -> Vec<i16> {
    assert!(wav.len() >= 44, "WAV shorter than a 44-byte header");
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect()
}

fn rms_f32(s: &[f32]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / s.len() as f64).sqrt()
}

/// Full-signal, zero-lag, normalized cross-correlation `dot(a,b)/(‖a‖·‖b‖)` over
/// equal-length vectors (D7). Returns 0.0 if either norm is zero.
fn norm_xcorr(a: &[i16], b: &[i16]) -> f64 {
    assert_eq!(a.len(), b.len(), "xcorr requires equal-length vectors");
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let (x, y) = (x as f64, y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// AC4 — a fake peer echoing injected audio → capture returns audio whose normalized
/// cross-correlation with the pipeline-matched reference is ≥ 0.90. Injected at 16 kHz
/// so the shim's resample is the documented identity passthrough (D1): the reference is
/// the source → i16 (no copied resampler), and the round trip is asserted BOTH exactly
/// (i16 body equality — a strong non-tautological plumbing proof) AND by the AC4-literal
/// correlation ≥ 0.90.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn audio_round_trip_echo_correlation() {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-audio-echo".into());
    register_audio_shim(
        &mgr,
        &id,
        user_data_dir.path(),
        &[("LOOM_FAKE_CHROMIUM_AUDIO_ECHO", "1")],
    );

    navigate_audio_session(&mgr, &id).await;

    // 440 Hz, amp 0.5, 16 kHz, 100 ms = 1600 samples (D7). 440 Hz ≪ 8 kHz Nyquist.
    let src = sine(440.0, 0.5, 16_000, 100);
    let reference: Vec<i16> = src.iter().map(|&s| f32_to_i16_ref(s)).collect();

    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        mgr.send_start_audio_capture(id.clone(), 0, 0, 0, 0)
            .await
            .expect("start_audio_capture errored");
        mgr.send_inject_audio(id.clone(), 0, 0, f32_le_bytes(&src), false)
            .await
            .expect("inject_audio errored");
        mgr.send_stop_audio_capture(id.clone(), 0, 0)
            .await
            .expect("stop_audio_capture errored")
    })
    .await
    .expect("audio round-trip did not complete within 30s");

    assert_eq!(outcome.stop_reason, "explicit", "clean stop, no cap hit");
    assert_eq!(outcome.dropped_frames, 0, "AC4/A7: no dropped frames");
    assert_eq!(
        outcome.source_sample_rate, 16_000,
        "native rate reported by the echo peer"
    );
    // Canonical 16 kHz / mono / 16-bit WAV header.
    let wav = &outcome.wav_bytes;
    assert!(
        wav.len() >= 44,
        "expected a valid WAV (>=44-byte header), got {} bytes (stop_reason={}, error={:?})",
        wav.len(),
        outcome.stop_reason,
        outcome.error
    );
    assert_eq!(
        u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
        16_000,
        "capture WAV rate"
    );
    assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
    assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "16-bit");

    let captured = wav_body_i16(wav);
    assert_eq!(
        captured.len(),
        reference.len(),
        "round trip preserved the sample count (16k passthrough)"
    );
    // Strong plumbing proof: passthrough echo must reproduce the samples exactly.
    assert_eq!(
        captured, reference,
        "captured body must equal the injected samples through the round trip"
    );
    // AC4 literal: normalized cross-correlation ≥ 0.90.
    let xcorr = norm_xcorr(&captured, &reference);
    assert!(
        xcorr >= 0.90,
        "AC4 cross-correlation {xcorr} must be ≥ 0.90"
    );

    mgr.shutdown_session("test-session-audio-echo").await;
    drop(user_data_dir);
}

/// AC3-shape harness coverage (D2) — a canned 48 kHz sine drained through the REAL
/// shim resampler yields a valid 16 kHz WAV whose RMS is preserved (RMS is
/// resample-invariant, so this exercises the real 48k→16k resampler through the wire
/// WITHOUT copying it). AC3 fidelity itself is owned by task 05's unit tests; this is
/// the harness's e2e smoke of the tone hook.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn audio_capture_tone_wav_rms_e2e() {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-audio-tone".into());
    register_audio_shim(
        &mgr,
        &id,
        user_data_dir.path(),
        &[("LOOM_FAKE_CHROMIUM_AUDIO_TONE", "440:100")],
    );

    navigate_audio_session(&mgr, &id).await;

    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        mgr.send_start_audio_capture(id.clone(), 0, 0, 0, 0)
            .await
            .expect("start_audio_capture errored");
        mgr.send_stop_audio_capture(id.clone(), 0, 0)
            .await
            .expect("stop_audio_capture errored")
    })
    .await
    .expect("tone capture did not complete within 30s");

    assert_eq!(outcome.stop_reason, "explicit");
    assert_eq!(
        outcome.source_sample_rate, 48_000,
        "tone drained at the 48 kHz AudioContext rate"
    );
    let wav = &outcome.wav_bytes;
    assert!(
        wav.len() >= 44,
        "expected a valid WAV (>=44-byte header), got {} bytes (stop_reason={}, error={:?})",
        wav.len(),
        outcome.stop_reason,
        outcome.error
    );
    assert_eq!(
        u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
        16_000,
        "capture resampled to 16 kHz"
    );
    let captured = wav_body_i16(wav);
    assert!(!captured.is_empty(), "tone produced samples");
    // ~100 ms @ 16 kHz ≈ 1600 samples.
    assert!(
        (1500..=1700).contains(&captured.len()),
        "≈1600 samples after 48k→16k, got {}",
        captured.len()
    );
    // RMS of a 0.5-amplitude sine ≈ 0.5/√2 ≈ 0.3536, preserved across the resample.
    let as_f32: Vec<f32> = captured
        .iter()
        .map(|&s| s as f32 / i16::MAX as f32)
        .collect();
    let rms = rms_f32(&as_f32);
    let expected = 0.5 / std::f64::consts::SQRT_2;
    assert!(
        (rms - expected).abs() < 5e-2,
        "captured RMS {rms} should be ≈ {expected} (resample preserves RMS)"
    );

    mgr.shutdown_session("test-session-audio-tone").await;
    drop(user_data_dir);
}

/// AC10 — `inject_audio` on a page that never called `getUserMedia` returns a typed
/// `no_microphone_request` (mapped from the page rejection by `map_enqueue_exception`),
/// and the session stays usable (D6/D13). At the host-sender layer the typed KIND rides
/// the `LoomError` detail string, so we assert the detail carries `no_microphone_request`
/// and is NOT the `inject_failed:` fallback.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn inject_audio_missing_mic_returns_typed_error() {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-audio-nogum".into());
    register_audio_shim(
        &mgr,
        &id,
        user_data_dir.path(),
        &[("LOOM_FAKE_CHROMIUM_AUDIO_NO_GUM", "1")],
    );

    navigate_audio_session(&mgr, &id).await;

    let err = tokio::time::timeout(
        Duration::from_secs(30),
        mgr.send_inject_audio(id.clone(), 0, 0, vec![0u8; 64], false),
    )
    .await
    .expect("inject_audio did not return within 30s")
    .expect_err("inject on a page without getUserMedia must be a typed error");

    let msg = err.to_string();
    assert!(
        msg.contains("no_microphone_request"),
        "AC10: expected typed no_microphone_request, got: {msg}"
    );
    assert!(
        !msg.contains("inject_failed:"),
        "AC10: must be the typed kind, not the inject_failed fallback: {msg}"
    );

    // Session stays usable (D13): a follow-up navigate still succeeds.
    navigate_audio_session(&mgr, &id).await;

    mgr.shutdown_session("test-session-audio-nogum").await;
    drop(user_data_dir);
}
