//! R7 keystone spike: does the **pinned** Chromium expose `MediaStreamTrackProcessor`,
//! and does reading it actually yield `AudioData` carrying real PCM?
//!
//! The voice-call-io design (PRD D9) taps PCM via Breakout Box —
//! `new MediaStreamTrackProcessor({track})` -> `ReadableStream<AudioData>` — *because* it loads no
//! module URL and so has no page-CSP surface. If that API is missing, capture must fall back to
//! `AudioWorklet` + a blob-URL module, the page-CSP risk returns, and the capture/harness tasks
//! change shape. This test settles that before they are written.
//!
//! Two probes, because one `getUserMedia` probe cannot tell two very different failures apart:
//!
//!   A. **synthetic track** (`OscillatorNode` -> `createMediaStreamDestination`) — no microphone,
//!      no permission prompt. This is the R7 answer: only `missing_api` here means "use A8".
//!   B. **`getUserMedia({audio:true})`** under the fake-device flags — the production shape. A
//!      failure here is a flags/fake-device problem for later tasks, NOT an A8 trigger.
//!
//! A frame that reports `sampleRate: 0` — or one that is pure silence — would satisfy a naive
//! "we got an AudioData" check while carrying nothing the capture design can use. So each probe
//! scans frames until it observes a non-zero sample, and reports the peak amplitude it saw.
//!
//! The session runs `--no-determinism`: loom otherwise arms `Emulation.setVirtualTimePolicy`,
//! under which `await reader.read()` may never resolve.
//!
//! Gated on LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH, and hard-asserts that the browser IS the pinned
//! build — R7 is a claim about the Chromium loom ships, so a green run against some other browser
//! would prove nothing. There is deliberately no "allow unpinned" escape hatch.
//!
//! # Recorded answer
//!
//! Run 2026-07-09, macOS arm64, **Chromium 132.0.6834.57** (the pin):
//!
//! | probe | stage | sampleRate | frames | channels | format | peak | framesRead |
//! |---|---|---|---|---|---|---|---|
//! | A — synthetic oscillator | `ok` | 48000 | 480 | 2 | `f32-planar` | 1.0 | 1 |
//! | B — `getUserMedia` + fake device | `ok` | 48000 | 480 | 1 | `f32-planar` | 0.307 | 51 |
//!
//! **R7 is retired**: `MediaStreamTrackProcessor` is present and yields real PCM. PRD D9 holds and
//! the A8 (`AudioWorklet` + blob-URL) fallback is not needed.
//!
//! Two observations that constrain downstream tasks:
//!
//!   1. Chromium's fake audio device emits **~510 ms of leading silence** (51 × 480 frames @ 48 kHz)
//!      before its tone begins. A round-trip correlation must skip or tolerate that lead-in.
//!   2. The synthetic destination track is **stereo** while the fake mic is **mono** — D9's
//!      host-side mix-to-mono is load-bearing, not incidental.
//!
//! # Running it
//!
//! ```sh
//! cargo build --target wasm32-wasip2 -p loom-surface-web --release   # provision_web_world needs this
//! LOOM_LIVE_E2E=1 \
//!   LOOM_CHROMIUM_PATH="$HOME/.config/loom/chromium/chrome-mac/Chromium.app/Contents/MacOS/Chromium" \
//!   cargo test -p loom-cli --test live_voice_e2e -- --ignored --nocapture
//! ```
//!
//! (Linux: `…/chromium/chrome-linux/chrome`. Install either with `loom postinstall`.)
//!
//! # What is durable here, and what is scaffolding
//!
//! The `#[ignore]`d probe is a **one-shot instrument** — no CI job arms it (nothing sets
//! LOOM_LIVE_E2E), so it runs only when a human asks. Everything below `decision_model_tests` is
//! **permanent**: those tests run in ordinary CI with no browser, and they pin the semantics that
//! keep a `no_frame` from ever being misread as "the API is missing". `pin_change_forces_spike_rerun`
//! is the tripwire that fails when `chromium_pin.rs` moves, so the recorded answer above cannot go
//! silently stale on a version bump.
//!
//! Findings are written to `target/loom-voice-spike-result.json` as well as stderr (a passing Rust
//! test's stderr is swallowed without `--nocapture`). `target/` is disposable, which is exactly why
//! the answer is also recorded in this doc comment — the only tracked, reviewed home it has.
//!
//! Note on `--profile standard`: the spike needs it because it evaluates an arbitrary async IIFE,
//! which the default `safe` profile's JS denylist blocks. Production audio capture (tasks 03–07)
//! must NOT require `standard` — trading the page-CSP risk for unrestricted page JS would be a bad
//! bargain, and nothing here licenses it.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;

use common::daemon_test_harness::DaemonTestHarness;
use loom_cli::chromium_pin::CHROMIUM_VERSION;
use serde::Deserialize;

/// Base flags keep real Chrome non-interactive (as in every sibling live test); the three media
/// flags let headless Chromium answer `getUserMedia` with a synthetic device, and — via
/// `--autoplay-policy` — let an `AudioContext` start without a user gesture.
/// `LOOM_CHROMIUM_EXTRA_FLAGS` is whitespace-split by the supervisor, so no spaces in values.
const CHROMIUM_FLAGS: &str = "--no-sandbox --disable-dev-shm-usage --use-mock-keychain \
     --password-store=basic --use-fake-device-for-media-stream \
     --use-fake-ui-for-media-stream --autoplay-policy=no-user-gesture-required";

const FIXTURE_HTML: &str =
    "<!doctype html><meta charset=utf-8><title>loom voice spike</title><body>probe</body>";

/// Bound every await inside the page. The shim's `recv_timeout_ms` default is 30_000
/// (`loom-host/src/shim_manager/types.rs:48`), so acquisition + read must stay well under it or a
/// hang surfaces as an opaque evaluate timeout instead of a typed `stage`.
const ACQUIRE_BUDGET_MS: u64 = 5_000;
const READ_BUDGET_SYNTHETIC_MS: u64 = 5_000;
/// The fake device emits a tone with periodic gaps, so give probe B longer to observe a non-zero
/// sample before concluding silence. 5_000 + 10_000 < 30_000.
const READ_BUDGET_GUM_MS: u64 = 10_000;

/// Runaway guard only — never the thing that ends a scan. It counts `AudioData` *chunks*, whose
/// duration is decided by the browser, so it must be large enough that even the shortest plausible
/// chunk cannot exhaust it within the read deadline. Worst case considered: 128 frames at 192 kHz =
/// 0.67 ms/chunk, so this covers >60 s — far beyond the 10 s budget. The loop cannot spin without
/// advancing time (every iteration awaits a deadline-bounded read), so this only guards against a
/// pathological clock. `stoppedBy` reports a cap hit regardless, so truncation is never silent.
const MAX_FRAMES_SCANNED: u64 = 100_000;
/// The shortest `AudioData` chunk we are willing to assume a browser might emit, used only to prove
/// the cap cannot preempt the deadline. Do not assume 480-frame / 10 ms chunks: that holds for the
/// pinned build today and is not a contract.
const WORST_CASE_CHUNK_MS: f64 = 128.0 / 192_000.0 * 1000.0;

/// The Chromium build the recorded answer in this file's doc comment was measured against.
/// `pin_change_forces_spike_rerun` compares it to the live pin.
const RECORDED_ANSWER_FOR_CHROMIUM: &str = "132.0.6834.57";

const SPIKE_RESULT_FILE: &str = "target/loom-voice-spike-result.json";

/// Acquire a track via `__FACTORY__`, run it through `MediaStreamTrackProcessor`, and report the
/// first `AudioData` frame plus the peak amplitude observed.
///
/// Returns a JSON string with a `stage` discriminator rather than throwing: an uncaught rejection
/// would surface as an opaque `js_throw` and destroy the verdict mapping. Every await is bounded —
/// notably track acquisition, since `AudioContext.resume()` can stay pending forever if the
/// autoplay policy is not lifted, which would otherwise hang the whole evaluate.
const PROBE_TEMPLATE: &str = r#"
(async () => {
  const FAIL_STAGE = "__FAIL_STAGE__";
  const ACQUIRE_MS = __ACQUIRE_BUDGET_MS__;
  const READ_MS = __READ_BUDGET_MS__;
  const MAX_FRAMES = __MAX_FRAMES__;
    const msg = (e) => String((e && (e.stack || e.message || e.name)) || e);
  // `wrapped` NEVER rejects: a losing racer whose promise later rejects would otherwise surface as
  // an unhandled rejection after we have already returned. Errors come back as `{err}`.
  const withTimeout = (p, ms) => Promise.race([
    Promise.resolve(p).then((v) => ({ v }), (e) => ({ err: e })),
    new Promise((r) => setTimeout(() => r({ timedOut: true }), Math.max(0, ms))),
  ]);
  // performance.now() is monotonic; Date.now() can step backwards under NTP and would
  // silently extend or truncate the read window.
  const now = () => performance.now();
  try {
    if (typeof MediaStreamTrackProcessor === 'undefined' || typeof AudioData === 'undefined') {
      return JSON.stringify({ stage: 'missing_api' });
    }

    const acqR = await withTimeout((__FACTORY__)(), ACQUIRE_MS);
    if (acqR.timedOut) return JSON.stringify({ stage: FAIL_STAGE, message: 'track acquisition timed out' });
    if (acqR.err) return JSON.stringify({ stage: FAIL_STAGE, message: msg(acqR.err) });
    const acq = acqR.v;

    const track = acq && acq.track;
    if (!track) return JSON.stringify({ stage: FAIL_STAGE, message: 'stream carried no audio track' });

    const reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
    const deadline = now() + READ_MS;
    let first = null, framesRead = 0, peak = 0, stoppedBy = 'deadline';

    while (true) {
      if (now() >= deadline) { stoppedBy = 'deadline'; break; }
      if (framesRead >= MAX_FRAMES) { stoppedBy = 'frame_cap'; break; }
      const r = await withTimeout(reader.read(), deadline - now());
      if (r.timedOut) { stoppedBy = 'deadline'; break; }
      if (r.err) { stoppedBy = 'read_error'; return JSON.stringify({ stage: 'error', message: msg(r.err), framesRead: framesRead, stoppedBy: stoppedBy }); }
      const frame = r.v && r.v.value;
      if (!frame || r.v.done) { stoppedBy = 'stream_ended'; break; }
      framesRead++;
      if (!first) {
        first = {
          sampleRate: frame.sampleRate,
          numberOfFrames: frame.numberOfFrames,
          numberOfChannels: frame.numberOfChannels,
          format: String(frame.format),
        };
      }
      try {
        // Plane 0 only. For a stereo source that means the left channel; a source with signal on
        // the right channel alone would read as silence. Neither an oscillator fed into a
        // destination node nor a mono mic can do that, so plane 0 is sufficient here.
        const opts = { planeIndex: 0, format: 'f32-planar' };
        const buf = new Float32Array(frame.allocationSize(opts) / 4);
        frame.copyTo(buf, opts);
        for (let i = 0; i < buf.length; i++) {
          const a = Math.abs(buf[i]);
          if (a > peak) peak = a;
        }
      } catch (e) { /* not f32-convertible; leave peak as observed */ }
      frame.close();
      if (peak > 0) { stoppedBy = 'peak'; break; }
    }

    try { await reader.cancel(); } catch (e) { /* already closed */ }
    try { track.stop(); } catch (e) { /* already ended */ }
    try { if (acq.cleanup) await acq.cleanup(); } catch (e) { /* best effort */ }

    if (!first) return JSON.stringify({ stage: 'no_frame', framesRead: framesRead, stoppedBy: stoppedBy });
    return JSON.stringify(Object.assign(
      { stage: 'ok', peak: peak, framesRead: framesRead, stoppedBy: stoppedBy }, first));
  } catch (e) {
    return JSON.stringify({ stage: 'error', message: msg(e) });
  }
})()
"#;

/// Probe A: a track with no microphone anywhere in the picture.
const FACTORY_SYNTHETIC: &str = r#"async () => {
  const ctx = new AudioContext();
  if (ctx.state === 'suspended') await ctx.resume();
  const osc = ctx.createOscillator();
  const dest = ctx.createMediaStreamDestination();
  osc.connect(dest);
  osc.start();
  return {
    track: dest.stream.getAudioTracks()[0],
    cleanup: async () => { try { osc.stop(); } catch (e) {} await ctx.close(); },
  };
}"#;

/// Probe B: the production shape — the page asks for a mic, Chromium hands it a fake device.
const FACTORY_GET_USER_MEDIA: &str = r#"async () => {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    throw new Error('navigator.mediaDevices unavailable (insecure context?)');
  }
  const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  return {
    track: stream.getAudioTracks()[0],
    cleanup: async () => { stream.getTracks().forEach((t) => t.stop()); },
  };
}"#;

fn probe_expr(factory: &str, fail_stage: &str, read_budget_ms: u64) -> String {
    PROBE_TEMPLATE
        .replace("__FACTORY__", factory)
        .replace("__FAIL_STAGE__", fail_stage)
        .replace("__ACQUIRE_BUDGET_MS__", &ACQUIRE_BUDGET_MS.to_string())
        .replace("__READ_BUDGET_MS__", &read_budget_ms.to_string())
        .replace("__MAX_FRAMES__", &MAX_FRAMES_SCANNED.to_string())
}

// ── the decision model (pure; unit-tested below without a browser) ───────────

#[derive(Debug, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ProbeResult {
    stage: String,
    #[serde(default)]
    sample_rate: f64,
    #[serde(default)]
    number_of_frames: f64,
    #[serde(default)]
    number_of_channels: f64,
    #[serde(default)]
    format: String,
    #[serde(default)]
    peak: f64,
    #[serde(default)]
    frames_read: u64,
    /// Which condition ended the frame scan: `peak` | `deadline` | `frame_cap` | `stream_ended`.
    /// Reported so a truncated scan can never be mistaken for a device that emitted only silence.
    #[serde(default)]
    stopped_by: String,
    #[serde(default)]
    message: String,
}

/// What a probe's `stage` means for the plan. Only `R7Realized` licenses the A8 rewrite.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The API exists and produced frames.
    Pass,
    /// `MediaStreamTrackProcessor`/`AudioData` absent. PRD D9 is invalid; fall back to A8.
    R7Realized,
    /// Something went wrong that does NOT tell us whether the API works. Never conclude A8.
    Inconclusive(String),
    /// The API works; the microphone path does not. Lands on tasks 03/07, not on capture design.
    MicPathBroken(String),
}

/// Probe A touches no device and no permission, so its stages map straight onto the R7 question.
/// A `no_frame` here is emphatically NOT "the API is missing" — it is an environment failure
/// (suspended AudioContext, starved timer) and must not trigger the A8 rewrite.
fn verdict_probe_a(p: &ProbeResult) -> Verdict {
    match p.stage.as_str() {
        "ok" => Verdict::Pass,
        "missing_api" => Verdict::R7Realized,
        "no_frame" => Verdict::Inconclusive(format!(
            "synthetic oscillator track produced no AudioData in {READ_BUDGET_SYNTHETIC_MS}ms \
             (frames_read={}). This is device-independent, so it does NOT establish that \
             MediaStreamTrackProcessor is missing — suspect a suspended AudioContext or a starved \
             timer. Investigate before concluding anything about R7.",
            p.frames_read
        )),
        "no_track" => Verdict::Inconclusive(format!(
            "createMediaStreamDestination yielded no audio track: {}",
            p.message
        )),
        other => Verdict::Inconclusive(format!("stage={other}: {}", p.message)),
    }
}

/// Probe B runs only after probe A has already answered R7, so nothing here can imply A8.
fn verdict_probe_b(p: &ProbeResult) -> Verdict {
    match p.stage.as_str() {
        "ok" => Verdict::Pass,
        "missing_api" => Verdict::Inconclusive(
            "probe B reports missing_api after probe A observed the API — contradictory; \
             re-run before trusting either result"
                .to_string(),
        ),
        "gum_failed" => Verdict::MicPathBroken(format!("getUserMedia failed: {}", p.message)),
        "no_frame" => Verdict::MicPathBroken(format!(
            "the fake audio device delivered a track but no AudioData in {READ_BUDGET_GUM_MS}ms \
             (frames_read={})",
            p.frames_read
        )),
        other => Verdict::Inconclusive(format!("stage={other}: {}", p.message)),
    }
}

/// `stage == "ok"` alone would accept a single silent 1-frame buffer at an absurd rate — exactly
/// the "the device produced something useless" case the capture design must not be built on.
fn frame_plausibility(p: &ProbeResult) -> Result<(), String> {
    if !(8_000.0..=192_000.0).contains(&p.sample_rate) {
        return Err(format!(
            "sampleRate {} outside the plausible 8000..=192000 Hz band",
            p.sample_rate
        ));
    }
    if p.number_of_frames < 1.0 {
        return Err(format!("numberOfFrames {} < 1", p.number_of_frames));
    }
    if p.number_of_channels < 1.0 {
        return Err(format!("numberOfChannels {} < 1", p.number_of_channels));
    }
    if p.peak <= 0.0 {
        // Naming `stopped_by` matters: a `frame_cap` stop means the scan was truncated and the
        // device may simply have a longer lead-in silence than the cap allows — a very different
        // problem from a device that genuinely emits nothing.
        return Err(format!(
            "every one of the {} frame(s) read was pure silence (peak == 0, scan ended by \
             '{}'); an AudioData carrying no signal cannot validate the capture design. If \
             stopped_by == 'frame_cap', raise MAX_FRAMES_SCANNED before concluding anything.",
            p.frames_read, p.stopped_by
        ));
    }
    Ok(())
}

/// `web.evaluate` surfaces the value on the receipt as `return_value_json`: JSON text which, for
/// our string-returning IIFE, *contains* a JSON string. Decode twice.
fn decode_probe(receipt: &serde_json::Value) -> Result<ProbeResult, String> {
    let raw = receipt["return_value_json"]
        .as_str()
        .ok_or_else(|| format!("receipt has no string `return_value_json`: {receipt}"))?;
    let once: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("return_value_json not JSON: {e}"))?;
    let inner = match once {
        serde_json::Value::String(s) => {
            serde_json::from_str(&s).map_err(|e| format!("inner payload not JSON: {e}"))?
        }
        // Tolerate a host that already unwrapped one level — but only an object. Accepting any
        // other shape here would let a malformed payload through as a defaulted ProbeResult.
        obj @ serde_json::Value::Object(_) => obj,
        other => {
            return Err(format!(
                "probe payload is neither a JSON string nor an object: {other}"
            ))
        }
    };
    serde_json::from_value(inner).map_err(|e| format!("probe payload has unexpected shape: {e}"))
}

// ── the live spike ───────────────────────────────────────────────────────────

#[test]
#[ignore = "real Chromium; gated on LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH"]
fn pinned_chromium_exposes_media_stream_track_processor() {
    if std::env::var("LOOM_LIVE_E2E").as_deref() != Ok("1") {
        eprintln!("skip: set LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH to run");
        return;
    }
    let chromium = match std::env::var("LOOM_CHROMIUM_PATH") {
        Ok(p) if Path::new(&p).exists() => p,
        _ => {
            eprintln!("skip: LOOM_CHROMIUM_PATH unset/missing");
            return;
        }
    };

    // No receipt, manifest, or session-create response carries a browser version, so the spike
    // must capture it itself — otherwise a green run says nothing about *which* Chromium answered.
    let reported_version = assert_pinned_build(&chromium);

    let mut harness = DaemonTestHarness::new()
        .env("LOOM_CHROMIUM_PATH", &chromium)
        .env("LOOM_CHROMIUM_EXTRA_FLAGS", CHROMIUM_FLAGS)
        .with_ready_timeout(std::time::Duration::from_secs(30));
    provision_web_world(harness.home());
    // A panic here (missing daemon binary, bad socket) is the intended behavior, matching every
    // sibling live test: if you armed the spike, a broken harness must be loud, not skipped.
    harness.start();

    // `standard` lifts the JS denylist; `--no-determinism` keeps virtual time from freezing the
    // async stream read (loom arms setVirtualTimePolicy whenever determinism is enabled).
    let sid = {
        let out = run_loom(
            &harness,
            &[
                "session",
                "create",
                "--profile",
                "standard",
                "--no-determinism",
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "session create not JSON: {e}; status={} stderr={:?}",
                out.status, out.stderr
            )
        });
        v["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session_id in {v}"))
            .to_string()
    };

    // loom's URL allowlist forbids `data:`; getUserMedia needs a secure context. 127.0.0.1 is both.
    let url = serve(FIXTURE_HTML);
    let out = run_loom(
        &harness,
        &[
            "action",
            "web.navigate",
            "--session",
            &sid,
            "--url",
            &url,
            "--until",
            "load",
        ],
    );
    let receipt: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "navigate not JSON: {e}; status={} stderr={:?}",
            out.status, out.stderr
        )
    });
    assert_eq!(
        receipt["status"], "success",
        "fixture navigate must succeed; got {receipt}"
    );

    let probe_a = evaluate_probe(
        &harness,
        &sid,
        &probe_expr(FACTORY_SYNTHETIC, "no_track", READ_BUDGET_SYNTHETIC_MS),
    );
    let probe_b = evaluate_probe(
        &harness,
        &sid,
        &probe_expr(FACTORY_GET_USER_MEDIA, "gum_failed", READ_BUDGET_GUM_MS),
    );

    let verdict_a = verdict_probe_a(&probe_a);
    let verdict_b = verdict_probe_b(&probe_b);
    // Plausibility BEFORE the artifact: `stage == "ok"` alone does not mean R7 is resolved, and a
    // durable record claiming `r7_resolved: true` about an unusable frame is worse than none.
    let plausible_a = frame_plausibility(&probe_a);
    let plausible_b = frame_plausibility(&probe_b);
    let r7_resolved = verdict_a == Verdict::Pass && plausible_a.is_ok();
    write_result_artifact(
        &reported_version,
        &probe_a,
        &probe_b,
        &verdict_a,
        &verdict_b,
        &plausible_a,
        &plausible_b,
        r7_resolved,
    );
    let _ = run_loom(&harness, &["session", "close", &sid]);

    // ── Probe A is the R7 answer. It needs no device, no permission, no media flag. ──
    match &verdict_a {
        Verdict::Pass => {}
        Verdict::R7Realized => panic!(
            "\n\nR7 REALIZED — MediaStreamTrackProcessor / AudioData are absent from pinned \
             Chromium {CHROMIUM_VERSION}.\n\
             PRD D9 is invalid. Capture must fall back to A8 (AudioWorklet + blob-URL module), \
             which reopens the page-CSP risk (old R4). Tasks 05 (capture) and 07 (fake-chromium \
             harness) must be reshaped before they are written.\n\
             probe A = {probe_a:?}\n"
        ),
        Verdict::Inconclusive(why) | Verdict::MicPathBroken(why) => panic!(
            "\n\nR7 UNRESOLVED (inconclusive, NOT an A8 trigger) — {why}\n\
             Do not reshape tasks 05/07 on this result; fix the environment and re-run.\n\
             probe A = {probe_a:?}\n"
        ),
    }
    if let Err(why) = &plausible_a {
        panic!(
            "\n\nR7 UNRESOLVED — probe A returned stage=ok but the frame is not usable: {why}\n\
             probe A = {probe_a:?}\n"
        );
    }

    // ── Probe B cannot imply A8: probe A already observed the API working. ──
    match &verdict_b {
        Verdict::Pass => {}
        Verdict::MicPathBroken(why) => panic!(
            "\n\nMediaStreamTrackProcessor works (probe A passed), but the microphone path does \
             not on pinned Chromium {CHROMIUM_VERSION}: {why}\n\
             R7 is NOT realized and A8 is NOT triggered. This lands on task 03 (launch flags, \
             grantPermissions) and task 07 (fake-chromium audio harness), not on the capture \
             design.\n\
             flags = {CHROMIUM_FLAGS}\n\
             probe B = {probe_b:?}\n"
        ),
        Verdict::Inconclusive(why) => {
            panic!("\n\nProbe B inconclusive: {why}\nprobe B = {probe_b:?}\n")
        }
        // Unreachable by construction: `verdict_probe_b` never returns R7Realized, precisely so
        // that no microphone-path failure can ever license the A8 rewrite.
        Verdict::R7Realized => unreachable!("probe B must never imply the A8 fallback"),
    }
    if let Err(why) = &plausible_b {
        panic!(
            "\n\nThe fake audio device delivered an unusable frame: {why}\n\
             This is a task 03/07 problem, not an A8 trigger.\n\
             probe B = {probe_b:?}\n"
        );
    }

    eprintln!(
        "\n=== R7 spike result ===\n\
         chromium: {reported_version}\n\
         probe A (synthetic):    {probe_a:?}\n\
         probe B (getUserMedia): {probe_b:?}\n\
         => MediaStreamTrackProcessor is present and yields real PCM. PRD D9 holds; A8 not needed.\n\
         (also written to {SPIKE_RESULT_FILE})\n"
    );
}

/// A passing Rust test's stderr is swallowed without `--nocapture`, and no CI job runs this test.
/// The spike's whole product is its answer, so persist it where a human can find it — including,
/// and especially, on the failure paths, where `r7_resolved` must read `false` rather than be absent.
#[allow(clippy::too_many_arguments)]
fn write_result_artifact(
    version: &str,
    a: &ProbeResult,
    b: &ProbeResult,
    va: &Verdict,
    vb: &Verdict,
    plausible_a: &Result<(), String>,
    plausible_b: &Result<(), String>,
    r7_resolved: bool,
) {
    let plausibility = |r: &Result<(), String>| match r {
        Ok(()) => serde_json::Value::Null,
        Err(e) => serde_json::Value::String(e.clone()),
    };
    let payload = serde_json::json!({
        "chromium_reported": version,
        "chromium_pinned": CHROMIUM_VERSION,
        "chromium_flags": CHROMIUM_FLAGS,
        "probe_a_synthetic": {
            "stage": a.stage, "sampleRate": a.sample_rate, "numberOfFrames": a.number_of_frames,
            "numberOfChannels": a.number_of_channels, "format": a.format, "peak": a.peak,
            "framesRead": a.frames_read, "stoppedBy": a.stopped_by, "message": a.message,
            "verdict": format!("{va:?}"),
            "implausibility": plausibility(plausible_a),
        },
        "probe_b_get_user_media": {
            "stage": b.stage, "sampleRate": b.sample_rate, "numberOfFrames": b.number_of_frames,
            "numberOfChannels": b.number_of_channels, "format": b.format, "peak": b.peak,
            "framesRead": b.frames_read, "stoppedBy": b.stopped_by, "message": b.message,
            "verdict": format!("{vb:?}"),
            "implausibility": plausibility(plausible_b),
        },
        // Both conditions, not just the stage-based verdict: a frame can be `ok` and useless.
        "r7_resolved": r7_resolved,
        "a8_fallback_required": *va == Verdict::R7Realized,
        "mic_path_ok": *vb == Verdict::Pass && plausible_b.is_ok(),
    });
    let path = workspace_root().join(SPIKE_RESULT_FILE);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(
        &path,
        serde_json::to_vec_pretty(&payload).unwrap_or_default(),
    ) {
        Ok(()) => eprintln!("spike result written to {}", path.display()),
        Err(e) => eprintln!("WARNING: could not write {}: {e}", path.display()),
    }
}

/// R7 is a claim about *the pinned Chromium*. Probing whatever browser happens to be installed
/// would make a green run unfalsifiable, so match the version the way CI does. Deliberately no
/// env-var override: an "allow unpinned" switch is a silent downgrade path to a meaningless green.
fn assert_pinned_build(chromium: &str) -> String {
    let out = std::process::Command::new(chromium)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("could not run `{chromium} --version`: {e}"));
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
    eprintln!("chromium --version: {reported}");
    assert!(
        reported.contains(CHROMIUM_VERSION),
        "LOOM_CHROMIUM_PATH points at {reported:?}, not the pinned {CHROMIUM_VERSION}. R7 is a \
         claim about the pinned build, so this run would prove nothing. Install it with \
         `loom postinstall`."
    );
    reported
}

/// Never panics. A harness-level failure (evaluate timed out, receipt malformed, CLI died) becomes
/// `stage: "error"` and flows through the verdict mapping like any other outcome — so the durable
/// artifact is still written for exactly the ambiguous failures a reader most needs to see.
fn evaluate_probe(harness: &DaemonTestHarness, sid: &str, expression: &str) -> ProbeResult {
    let out = run_loom(
        harness,
        &[
            "action",
            "web.evaluate",
            "--session",
            sid,
            "--expression",
            expression,
        ],
    );
    let harness_error = |detail: String| ProbeResult {
        stage: "error".into(),
        message: format!(
            "{detail} (exit={} stdout={:?} stderr={:?})",
            out.status,
            truncate(&out.stdout, 400),
            truncate(&out.stderr, 400)
        ),
        ..Default::default()
    };
    let receipt: serde_json::Value = match serde_json::from_str(&out.stdout) {
        Ok(v) => v,
        Err(e) => return harness_error(format!("evaluate stdout not JSON: {e}")),
    };
    match decode_probe(&receipt) {
        Ok(p) => p,
        Err(e) => harness_error(e),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…[{} more bytes]", &s[..max], s.len() - max)
    }
}

// ── helpers (per-file copies, as in every sibling live_*.rs) ─────────────────

struct CliOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run_loom(harness: &DaemonTestHarness, args: &[&str]) -> CliOutput {
    let mut cmd = harness.loom_command();
    cmd.arg("--json");
    cmd.args(args);
    let out = cmd.output().expect("spawn loom CLI");
    CliOutput {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Serve `body` on a fresh ephemeral 127.0.0.1 port; return the http:// URL.
///
/// Each connection is handled on its own thread: Chromium opens speculative pre-connections that
/// may never send a byte, and a single-threaded accept loop would wedge behind one of them. The
/// request is drained until the header terminator (bounded, with a read timeout) before replying,
/// so a large or segmented header set cannot make us answer early and reset the connection.
fn serve(body: &'static str) -> String {
    const MAX_REQUEST_BYTES: usize = 64 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            req.extend_from_slice(&buf[..n]);
                            if req.windows(4).any(|w| w == b"\r\n\r\n")
                                || req.len() >= MAX_REQUEST_BYTES
                            {
                                break;
                            }
                        }
                        // Timed out or errored: a pre-connection that never spoke. Drop it.
                        Err(_) => return,
                    }
                }
                if req.is_empty() {
                    return;
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

fn provision_web_world(home: &Path) {
    let cfg_loom = home.join(".config").join("loom");
    let surfaces_dir = cfg_loom.join("surfaces");
    std::fs::create_dir_all(&surfaces_dir).unwrap();
    std::os::unix::fs::symlink(cwasm_path(), surfaces_dir.join("loom_surface_web.cwasm")).unwrap();
    let schemas_dir = cfg_loom.join("schemas").join("v1");
    std::fs::create_dir_all(&schemas_dir).unwrap();
    loom_cli::postinstall_runner::schema_step(&schemas_dir).unwrap();
    let permissive = r#"{"request":{"type":"object","additionalProperties":true},"response":{"type":"object","additionalProperties":true}}"#;
    for m in [
        "session.create",
        "session.close",
        "session.list",
        "session.validate",
    ] {
        std::fs::write(schemas_dir.join(format!("{m}.json")), permissive).unwrap();
    }
}

fn cwasm_path() -> &'static Path {
    static CWASM: OnceLock<PathBuf> = OnceLock::new();
    CWASM.get_or_init(|| {
        let wasm = workspace_root().join("target/wasm32-wasip2/release/loom_surface_web.wasm");
        assert!(
            wasm.exists(),
            "build: cargo build --target wasm32-wasip2 -p loom-surface-web --release"
        );
        // Distinct from the sibling live tests' cache dirs: nextest runs each test in its own
        // process, so a shared path would race.
        let cwasm = workspace_root().join("target/loom-live-voice-cwasm/loom_surface_web.cwasm");
        std::fs::create_dir_all(cwasm.parent().unwrap()).unwrap();
        if !cwasm.exists() {
            use loom_host::compiler::Compiler;
            use loom_host::wasm_runtime::{WasmRuntime, WasmRuntimeConfig};
            let rt = WasmRuntime::new(WasmRuntimeConfig::default()).unwrap();
            Compiler::new(rt).compile_module(&wasm, &cwasm).unwrap();
        }
        cwasm
    })
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

// ── task 04 (Inject) real-browser AC1 proof ──────────────────────────────────
//
// The TestCdp unit tests prove inject issues the correct CDP calls; this proves
// the whole chain actually lands audio on the synthetic mic track in the pinned
// Chromium: an `--audio` session's `getUserMedia({audio:true})` returns loom's
// synthetic track, `web.inject_audio` enqueues a loud tone into it, and a
// `MediaStreamTrackProcessor` tap on that same track then reads a non-silent
// peak. Gated on LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH, like the R7 spike.

/// Minimal standard-alphabet base64 (no dep) for the inline WAV payload.
fn base64_encode(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A ~0.3 s 16 kHz mono i16 WAV of a loud 440 Hz sine (amplitude ~0.6 full-scale)
/// — clearly non-silent so the tap's peak assertion is unambiguous.
fn tone_wav() -> Vec<u8> {
    let rate: u32 = 16_000;
    let n: u32 = rate * 3 / 10; // 0.3 s
    let mut pcm = Vec::with_capacity(n as usize * 2);
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.6;
        pcm.extend_from_slice(&((s * i16::MAX as f64) as i16).to_le_bytes());
    }
    let data_len = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

/// Starts `getUserMedia({audio:true})` (→ loom's synthetic track via the task-03
/// override) and a persistent `MediaStreamTrackProcessor` loop that records the
/// running peak amplitude on `window.__loomInjectPeak`. Fire-and-forget: returns
/// immediately; the loop keeps running on the page event loop between actions.
const AC1_TAP_SETUP: &str = r#"
(async () => {
  window.__loomInjectPeak = 0;
  window.__loomTapError = null;
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const track = stream.getAudioTracks()[0];
    if (!track) { window.__loomTapError = 'no audio track'; return JSON.stringify({ ok: false }); }
    const reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
    (async () => {
      for (;;) {
        const { value: frame, done } = await reader.read();
        if (done || !frame) break;
        try {
          const opts = { planeIndex: 0, format: 'f32-planar' };
          const buf = new Float32Array(frame.allocationSize(opts) / 4);
          frame.copyTo(buf, opts);
          for (let i = 0; i < buf.length; i++) {
            const a = Math.abs(buf[i]);
            if (a > window.__loomInjectPeak) window.__loomInjectPeak = a;
          }
        } catch (e) { /* non-f32 frame; skip */ }
        frame.close();
      }
    })();
    return JSON.stringify({ ok: true });
  } catch (e) {
    window.__loomTapError = String((e && (e.message || e.name)) || e);
    return JSON.stringify({ ok: false });
  }
})()
"#;

#[test]
#[ignore = "real Chromium; gated on LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH"]
fn inject_audio_delivers_samples_to_mic_track() {
    if std::env::var("LOOM_LIVE_E2E").as_deref() != Ok("1") {
        eprintln!("skip: set LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH to run");
        return;
    }
    let chromium = match std::env::var("LOOM_CHROMIUM_PATH") {
        Ok(p) if Path::new(&p).exists() => p,
        _ => {
            eprintln!("skip: LOOM_CHROMIUM_PATH unset/missing");
            return;
        }
    };

    let mut harness = DaemonTestHarness::new()
        .env("LOOM_CHROMIUM_PATH", &chromium)
        .env("LOOM_CHROMIUM_EXTRA_FLAGS", CHROMIUM_FLAGS)
        .with_ready_timeout(std::time::Duration::from_secs(30));
    provision_web_world(harness.home());
    harness.start();

    // `--audio` installs the synthetic-mic bootstrap; `--no-determinism` keeps
    // virtual time from freezing the async tap loop; `standard` lifts the JS
    // denylist so the tap's evaluate isn't blocked.
    let sid = {
        let out = run_loom(
            &harness,
            &[
                "session",
                "create",
                "--profile",
                "standard",
                "--no-determinism",
                "--audio",
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&out.stdout)
            .unwrap_or_else(|e| panic!("session create not JSON: {e}; stderr={:?}", out.stderr));
        v["session_id"].as_str().expect("session_id").to_string()
    };

    let url = serve(FIXTURE_HTML);
    let nav = run_loom(
        &harness,
        &[
            "action",
            "web.navigate",
            "--session",
            &sid,
            "--url",
            &url,
            "--until",
            "load",
        ],
    );
    let nav_receipt: serde_json::Value = serde_json::from_str(&nav.stdout)
        .unwrap_or_else(|e| panic!("navigate not JSON: {e}; stderr={:?}", nav.stderr));
    assert_eq!(
        nav_receipt["status"], "success",
        "navigate must succeed; got {nav_receipt}"
    );

    // Start the gUM tap (returns immediately; the read loop persists).
    let tap = run_loom(
        &harness,
        &[
            "action",
            "web.evaluate",
            "--session",
            &sid,
            "--expression",
            AC1_TAP_SETUP,
        ],
    );
    assert!(
        tap.stdout.contains("\"ok\":true") || tap.stdout.contains("ok\": true"),
        "gUM tap setup failed: stdout={} stderr={:?}",
        truncate(&tap.stdout, 400),
        tap.stderr
    );

    // Inject the loud tone; await_playout=true so the call returns only after the
    // clip has played through the destination node (samples definitely flowed).
    let b64 = base64_encode(&tone_wav());
    let inject = run_loom(
        &harness,
        &[
            "action",
            "web.inject_audio",
            "--session",
            &sid,
            "--audio_b64",
            &b64,
            "--await_playout",
            "true",
        ],
    );
    let inject_receipt: serde_json::Value =
        serde_json::from_str(&inject.stdout).unwrap_or_else(|e| {
            panic!(
                "inject not JSON: {e}; stdout={} stderr={:?}",
                inject.stdout, inject.stderr
            )
        });
    assert_eq!(
        inject_receipt["status"], "success",
        "inject_audio must succeed; got {inject_receipt}"
    );
    assert!(
        inject_receipt["outcome_hash"].as_str().is_some(),
        "inject receipt must carry the constant outcome_hash; got {inject_receipt}"
    );

    // The tap must have observed non-silent audio on the mic track.
    let peak = evaluate_probe(
        &harness,
        &sid,
        "JSON.stringify({ stage: 'ok', peak: window.__loomInjectPeak || 0, message: String(window.__loomTapError||'') })",
    );
    let _ = run_loom(&harness, &["session", "close", &sid]);

    assert!(
        peak.peak > 0.05,
        "injected tone must reach the mic track: observed peak={} (tap error={:?})",
        peak.peak,
        peak.message
    );
    eprintln!(
        "AC1 OK — injected tone reached the synthetic mic track, peak={}",
        peak.peak
    );
}

// ── #318 real-browser proof: a second call in the same document still has a microphone ──
//
// A voice-call page ends a call with `stream.getTracks().forEach((t) => t.stop())` — that is how a real app
// releases the microphone. The synthetic mic used to hand every getUserMedia the SAME destination track, so the
// page's stop() ended it for the whole document: the next getUserMedia returned that already-`ended` track and
// every later inject was spoken into a dead track while resolving ok (a hollie ATS voice spec's second call reached
// the agent with zero caller audio). This tap stops call 1's track first, then taps call 2's.
const SECOND_CALL_TAP_SETUP: &str = r#"
(async () => {
  window.__loomInjectPeak = 0;
  window.__loomTapError = null;
  try {
    const first = (await navigator.mediaDevices.getUserMedia({ audio: true })).getAudioTracks()[0];
    first.stop();
    const track = (await navigator.mediaDevices.getUserMedia({ audio: true })).getAudioTracks()[0];
    window.__loomSecondCall = { sameTrack: track === first, state: track ? track.readyState : 'none' };
    if (!track) { window.__loomTapError = 'no audio track'; return JSON.stringify({ ok: false }); }
    const reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
    (async () => {
      for (;;) {
        const { value: frame, done } = await reader.read();
        if (done || !frame) break;
        try {
          const opts = { planeIndex: 0, format: 'f32-planar' };
          const buf = new Float32Array(frame.allocationSize(opts) / 4);
          frame.copyTo(buf, opts);
          for (let i = 0; i < buf.length; i++) {
            const a = Math.abs(buf[i]);
            if (a > window.__loomInjectPeak) window.__loomInjectPeak = a;
          }
        } catch (e) { /* non-f32 frame; skip */ }
        frame.close();
      }
    })();
    return JSON.stringify({ ok: true });
  } catch (e) {
    window.__loomTapError = String((e && (e.message || e.name)) || e);
    return JSON.stringify({ ok: false });
  }
})()
"#;

#[test]
#[ignore = "real Chromium; gated on LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH"]
fn inject_audio_reaches_a_second_call_after_the_page_stopped_the_first() {
    if std::env::var("LOOM_LIVE_E2E").as_deref() != Ok("1") {
        eprintln!("skip: set LOOM_LIVE_E2E=1 + LOOM_CHROMIUM_PATH to run");
        return;
    }
    let chromium = match std::env::var("LOOM_CHROMIUM_PATH") {
        Ok(p) if Path::new(&p).exists() => p,
        _ => {
            eprintln!("skip: LOOM_CHROMIUM_PATH unset/missing");
            return;
        }
    };
    let mut harness = DaemonTestHarness::new()
        .env("LOOM_CHROMIUM_PATH", &chromium)
        .env("LOOM_CHROMIUM_EXTRA_FLAGS", CHROMIUM_FLAGS)
        .with_ready_timeout(std::time::Duration::from_secs(30));
    provision_web_world(harness.home());
    harness.start();
    let sid = {
        let out = run_loom(
            &harness,
            &[
                "session",
                "create",
                "--profile",
                "standard",
                "--no-determinism",
                "--audio",
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&out.stdout)
            .unwrap_or_else(|e| panic!("session create not JSON: {e}; stderr={:?}", out.stderr));
        v["session_id"].as_str().expect("session_id").to_string()
    };
    let url = serve(FIXTURE_HTML);
    let nav = run_loom(
        &harness,
        &[
            "action",
            "web.navigate",
            "--session",
            &sid,
            "--url",
            &url,
            "--until",
            "load",
        ],
    );
    let nav_receipt: serde_json::Value = serde_json::from_str(&nav.stdout)
        .unwrap_or_else(|e| panic!("navigate not JSON: {e}; stderr={:?}", nav.stderr));
    assert_eq!(
        nav_receipt["status"], "success",
        "navigate must succeed; got {nav_receipt}"
    );
    let tap = run_loom(
        &harness,
        &[
            "action",
            "web.evaluate",
            "--session",
            &sid,
            "--expression",
            SECOND_CALL_TAP_SETUP,
        ],
    );
    assert!(
        tap.stdout.contains("\"ok\":true") || tap.stdout.contains("ok\": true"),
        "second-call tap setup failed: stdout={} stderr={:?}",
        truncate(&tap.stdout, 400),
        tap.stderr
    );
    let b64 = base64_encode(&tone_wav());
    let inject = run_loom(
        &harness,
        &[
            "action",
            "web.inject_audio",
            "--session",
            &sid,
            "--audio_b64",
            &b64,
            "--await_playout",
            "true",
        ],
    );
    let inject_receipt: serde_json::Value =
        serde_json::from_str(&inject.stdout).unwrap_or_else(|e| {
            panic!(
                "inject not JSON: {e}; stdout={} stderr={:?}",
                inject.stdout, inject.stderr
            )
        });
    assert_eq!(
        inject_receipt["status"], "success",
        "inject_audio must succeed; got {inject_receipt}"
    );
    let probe = evaluate_probe(
        &harness,
        &sid,
        "JSON.stringify({ stage: 'ok', peak: window.__loomInjectPeak || 0, \
         message: JSON.stringify(window.__loomSecondCall || {}) + ' ' + String(window.__loomTapError || '') })",
    );
    let _ = run_loom(&harness, &["session", "close", &sid]);
    assert!(
        probe.message.contains("\"sameTrack\":false")
            && probe.message.contains("\"state\":\"live\""),
        "call 2 must get a FRESH, live track, not call 1's stopped one: {}",
        probe.message
    );
    assert!(
        probe.peak > 0.05,
        "injected tone must reach call 2's track: observed peak={} ({})",
        probe.peak,
        probe.message
    );
    eprintln!(
        "#318 OK — call 2 got a fresh live track and the tone reached it, peak={}",
        probe.peak
    );
}

// ── decision-model tests (no browser; these run in normal CI) ────────────────
//
// The live probe cannot be red-then-green — the "implementation" is Chromium. These give the
// verdict mapping and the receipt decoding real, durable teeth instead of a one-off manual
// mutation check: they are what would catch a `no_frame` silently being read as "use A8".

#[cfg(test)]
mod decision_model_tests {
    use super::*;

    fn receipt_for(payload: &str) -> serde_json::Value {
        // The real shape: `return_value_json` is JSON text containing a JSON string.
        serde_json::json!({ "return_value_json": serde_json::to_string(payload).unwrap() })
    }

    #[test]
    fn decodes_the_double_encoded_return_value() {
        let r = receipt_for(
            r#"{"stage":"ok","sampleRate":48000,"numberOfFrames":480,"numberOfChannels":1,"format":"f32-planar","peak":0.5,"framesRead":1}"#,
        );
        let p = decode_probe(&r).expect("decode");
        assert_eq!(p.stage, "ok");
        assert_eq!(p.sample_rate, 48000.0);
        assert_eq!(p.number_of_frames, 480.0);
        assert_eq!(p.frames_read, 1);
        assert_eq!(p.peak, 0.5);
    }

    #[test]
    fn tolerates_an_already_unwrapped_object() {
        let r =
            serde_json::json!({ "return_value_json": r#"{"stage":"no_frame","framesRead":3}"# });
        let p = decode_probe(&r).expect("decode");
        assert_eq!(p.stage, "no_frame");
        assert_eq!(p.frames_read, 3);
    }

    #[test]
    fn missing_return_value_is_an_error_not_a_panic() {
        assert!(decode_probe(&serde_json::json!({ "status": "success" })).is_err());
    }

    /// A payload that is neither a JSON string nor an object must not be waved through as a
    /// defaulted `ProbeResult` — that would read as `stage: ""` and land in `Inconclusive`,
    /// hiding a malformed receipt behind a plausible-looking verdict.
    #[test]
    fn a_non_object_payload_is_rejected_rather_than_defaulted() {
        let r = serde_json::json!({ "return_value_json": "42" });
        let err = decode_probe(&r).expect_err("a bare number must not decode");
        assert!(err.contains("neither a JSON string nor an object"), "{err}");
    }

    /// The frame scan reports `stage: "ok"` for a probe that never yielded a usable frame only if
    /// `first` was set; an all-silence result must still be caught by plausibility, and the
    /// artifact must not be able to claim R7 resolved on it.
    #[test]
    fn r7_is_not_resolved_when_a_passing_stage_has_an_implausible_frame() {
        let silent = ProbeResult {
            peak: 0.0,
            stopped_by: "deadline".into(),
            ..good_frame()
        };
        assert_eq!(
            verdict_probe_a(&silent),
            Verdict::Pass,
            "stage alone passes"
        );
        assert!(
            frame_plausibility(&silent).is_err(),
            "but plausibility must reject it"
        );
        // This conjunction is exactly what the live test writes as `r7_resolved`.
        let r7_resolved =
            verdict_probe_a(&silent) == Verdict::Pass && frame_plausibility(&silent).is_ok();
        assert!(!r7_resolved, "artifact must not claim R7 resolved");
    }

    #[test]
    fn only_missing_api_on_probe_a_licenses_the_a8_fallback() {
        let missing = ProbeResult {
            stage: "missing_api".into(),
            ..Default::default()
        };
        assert_eq!(verdict_probe_a(&missing), Verdict::R7Realized);
    }

    /// The bug this whole mapping exists to prevent: a device-independent `no_frame` must never
    /// be reported as "the API is missing", or it would trigger an unnecessary A8 rewrite.
    #[test]
    fn probe_a_no_frame_is_inconclusive_never_r7_realized() {
        let no_frame = ProbeResult {
            stage: "no_frame".into(),
            frames_read: 0,
            ..Default::default()
        };
        match verdict_probe_a(&no_frame) {
            Verdict::Inconclusive(_) => {}
            other => panic!("no_frame must be inconclusive, got {other:?}"),
        }
        assert_ne!(verdict_probe_a(&no_frame), Verdict::R7Realized);
    }

    #[test]
    fn probe_b_failures_never_imply_a8() {
        for stage in ["gum_failed", "no_frame"] {
            let p = ProbeResult {
                stage: stage.into(),
                ..Default::default()
            };
            match verdict_probe_b(&p) {
                Verdict::MicPathBroken(_) => {}
                other => panic!("{stage} must be MicPathBroken, got {other:?}"),
            }
        }
        // A contradiction (B says missing_api after A saw it work) is inconclusive, not R7Realized.
        let contradictory = ProbeResult {
            stage: "missing_api".into(),
            ..Default::default()
        };
        assert!(matches!(
            verdict_probe_b(&contradictory),
            Verdict::Inconclusive(_)
        ));
    }

    #[test]
    fn both_probes_pass_on_ok() {
        let ok = ProbeResult {
            stage: "ok".into(),
            ..Default::default()
        };
        assert_eq!(verdict_probe_a(&ok), Verdict::Pass);
        assert_eq!(verdict_probe_b(&ok), Verdict::Pass);
    }

    fn good_frame() -> ProbeResult {
        ProbeResult {
            stage: "ok".into(),
            sample_rate: 48_000.0,
            number_of_frames: 480.0,
            number_of_channels: 1.0,
            format: "f32-planar".into(),
            peak: 0.31,
            frames_read: 1,
            stopped_by: "peak".into(),
            message: String::new(),
        }
    }

    #[test]
    fn a_plausible_frame_passes() {
        assert!(frame_plausibility(&good_frame()).is_ok());
    }

    /// FND-0018: `stage == "ok"` alone would wave through a silent, 1-frame, 8 kHz buffer.
    #[test]
    fn silence_is_rejected_even_when_the_stage_is_ok() {
        let silent = ProbeResult {
            peak: 0.0,
            frames_read: 42,
            ..good_frame()
        };
        let err = frame_plausibility(&silent).expect_err("pure silence must not pass");
        assert!(err.contains("silence"), "unexpected message: {err}");
    }

    #[test]
    fn implausible_frame_shapes_are_rejected() {
        for (bad, needle) in [
            (
                ProbeResult {
                    sample_rate: 0.0,
                    ..good_frame()
                },
                "sampleRate",
            ),
            (
                ProbeResult {
                    sample_rate: 1_000_000.0,
                    ..good_frame()
                },
                "sampleRate",
            ),
            (
                ProbeResult {
                    number_of_frames: 0.0,
                    ..good_frame()
                },
                "numberOfFrames",
            ),
            (
                ProbeResult {
                    number_of_channels: 0.0,
                    ..good_frame()
                },
                "numberOfChannels",
            ),
        ] {
            let err = frame_plausibility(&bad).expect_err("must reject");
            assert!(err.contains(needle), "expected {needle} in {err}");
        }
    }

    /// The probe JS must stay inside the shim's 30s `recv_timeout_ms` or a hang surfaces as an
    /// opaque evaluate timeout instead of a typed stage.
    #[test]
    fn probe_budgets_stay_under_the_shim_recv_timeout() {
        let worst = ACQUIRE_BUDGET_MS + READ_BUDGET_GUM_MS;
        assert!(
            worst < 30_000,
            "probe budget {worst}ms must stay under the 30_000ms shim recv timeout"
        );
    }

    /// The frame cap must never be the thing that ends a scan — otherwise a device with a longer
    /// lead-in silence than the cap gets misreported as "emitted only silence". At 480 frames per
    /// 10ms, the cap has to outlast the longest read budget.
    #[test]
    fn the_frame_cap_can_never_preempt_the_read_deadline() {
        // Deliberately NOT 480-frame/10ms chunks: chunk size is the browser's choice, and assuming
        // the pinned build's current value is how this guard would silently rot.
        let cap_covers_ms = MAX_FRAMES_SCANNED as f64 * WORST_CASE_CHUNK_MS;
        let longest_budget_ms = READ_BUDGET_GUM_MS.max(READ_BUDGET_SYNTHETIC_MS) as f64;
        assert!(
            cap_covers_ms > longest_budget_ms,
            "MAX_FRAMES_SCANNED covers {cap_covers_ms:.0}ms of audio at the worst-case chunk size \
             ({WORST_CASE_CHUNK_MS:.2}ms), but the read budget is up to {longest_budget_ms:.0}ms — \
             the cap would truncate the scan"
        );
    }

    /// Probe A's remaining stages are diagnostics, not verdicts about the API's existence.
    #[test]
    fn probe_a_no_track_and_error_are_inconclusive() {
        for stage in ["no_track", "error", "gum_failed", "something_unforeseen"] {
            let p = ProbeResult {
                stage: stage.into(),
                ..Default::default()
            };
            assert!(
                matches!(verdict_probe_a(&p), Verdict::Inconclusive(_)),
                "{stage} must be inconclusive on probe A"
            );
        }
    }

    /// A truncated scan and a genuinely silent device are different problems; the error must say
    /// which one it saw, or the reader will "fix" the wrong thing.
    #[test]
    fn a_silent_result_reports_what_ended_the_scan() {
        let truncated = ProbeResult {
            peak: 0.0,
            frames_read: MAX_FRAMES_SCANNED,
            stopped_by: "frame_cap".into(),
            ..good_frame()
        };
        let err = frame_plausibility(&truncated).expect_err("silence must not pass");
        assert!(
            err.contains("frame_cap"),
            "must name the stop reason: {err}"
        );
        assert!(
            err.contains("MAX_FRAMES_SCANNED"),
            "must tell the operator what to raise: {err}"
        );
    }

    /// The tripwire. The recorded answer in this file's doc comment describes ONE Chromium build.
    /// No CI job arms the live probe, so if the pin moves and nothing fails, the answer silently
    /// goes stale. This test is that failure.
    #[test]
    fn pin_change_forces_spike_rerun() {
        assert_eq!(
            CHROMIUM_VERSION, RECORDED_ANSWER_FOR_CHROMIUM,
            "the pinned Chromium moved to {CHROMIUM_VERSION}, but the R7 answer recorded in this \
             file's doc comment was measured against {RECORDED_ANSWER_FOR_CHROMIUM}, and no CI job \
             re-verifies it. Bumping the pin is already a deliberate edit (version + four \
             per-platform SHA-256s in chromium_pin.rs), so re-running one spike is proportionate: \
             see `Running it` in this file's docs, update the recorded table, then update \
             RECORDED_ANSWER_FOR_CHROMIUM. Do not just change this constant — the whole point is \
             that the answer is about a specific browser build."
        );
    }

    #[test]
    fn probe_expr_substitutes_every_placeholder() {
        let expr = probe_expr(FACTORY_SYNTHETIC, "no_track", READ_BUDGET_SYNTHETIC_MS);
        assert!(!expr.contains("__FACTORY__"));
        assert!(!expr.contains("__FAIL_STAGE__"));
        assert!(!expr.contains("__ACQUIRE_BUDGET_MS__"));
        assert!(!expr.contains("__READ_BUDGET_MS__"));
        assert!(expr.contains("createMediaStreamDestination"));
        assert!(expr.contains("\"no_track\""));
    }

    /// The media flags are the only reason headless Chromium can answer getUserMedia, and the
    /// supervisor splits this string on whitespace — a value containing a space would silently
    /// become two flags.
    #[test]
    fn chromium_flags_are_whitespace_split_safe_and_carry_the_media_flags() {
        for f in [
            "--use-fake-device-for-media-stream",
            "--use-fake-ui-for-media-stream",
            "--autoplay-policy=no-user-gesture-required",
        ] {
            assert!(
                CHROMIUM_FLAGS.split_whitespace().any(|t| t == f),
                "missing {f}"
            );
        }
        assert!(CHROMIUM_FLAGS
            .split_whitespace()
            .all(|t| t.starts_with("--")));
    }
}
