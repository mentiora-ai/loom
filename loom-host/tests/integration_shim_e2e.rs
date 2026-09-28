//! Full host→shim end-to-end integration test.
//!
//! Drives `ShimManager::send` against the real `loom-shim-chromium`
//! binary, which spawns the test-only `fake-chromium` binary that
//! simulates a real Chromium DevTools endpoint. Validates the chain:
//!
//!   ShimManager.send(ShimId, opaque_cbor)
//!     → spawn loom-shim-chromium child via socketpair + LOOM_SHIM_FD=3
//!     → loom-shim-chromium spawns fake-chromium subprocess
//!     → fake-chromium prints "DevTools listening on ws://127.0.0.1:N/.."
//!     → loom-shim-chromium's ChromiumSupervisor::start parses the URL
//!     → ChromiumCdpConnection connects via tokio-tungstenite
//!     → ShimDispatcher routes ShimRequest::CdpSend → cdp.command
//!     → fake-chromium responds with canned JSON
//!     → ShimResponse::Ok flows back through the demux loop
//!     → ShimManager::send returns the re-encoded payload bytes
//!
//! Run:
//!   `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium`
//!   `cargo test -p loom-host --test integration_shim_e2e -- --ignored`
//!
//! Marked `#[ignore]` so a default `cargo test --workspace` doesn't
//! force the fake-chromium build. The ignore is opt-in so CI can run
//! it after building the harness binary.

#![cfg(unix)]

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use loom_shared::shim_protocol::{ciborium_to_vec, CdpMessage};
use std::time::Duration;

/// Locate the loom-shims target binaries. They live in another crate's
/// bin slot so CARGO_BIN_EXE_* isn't available — use the test binary's
/// own location to find the cargo target dir.
fn target_bin_dir() -> std::path::PathBuf {
    // The test binary itself lives at `<target>/debug/deps/<test>-<hash>`.
    let test_exe = std::env::current_exe().expect("current_exe");
    let deps = test_exe.parent().expect("deps dir");
    deps.parent().expect("debug dir").to_path_buf()
}

fn shim_bin() -> String {
    target_bin_dir()
        .join("loom-shim-chromium")
        .to_string_lossy()
        .into_owned()
}

fn fake_chromium_bin() -> String {
    target_bin_dir()
        .join("fake-chromium")
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn host_to_shim_to_fake_chromium_round_trip() {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId("chromium:test-session-1".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    // Build a Page.navigate CdpMessage and ciborium-encode it.
    // ShimManager::send wraps this in ShimRequest::CdpSend.
    let navigate_msg = CdpMessage {
        method: "Page.navigate".into(),
        params: ciborium::value::Value::Map(vec![
            (
                ciborium::value::Value::Text("url".into()),
                ciborium::value::Value::Text("https://example.com".into()),
            ),
            (
                ciborium::value::Value::Text("transitionType".into()),
                ciborium::value::Value::Text("typed".into()),
            ),
        ]),
    };
    let payload_bytes = ciborium_to_vec(&navigate_msg).expect("encode CdpMessage");

    // The chain: ShimManager.spawn → loom-shim-chromium binary →
    // fake-chromium subprocess → ws connect → CdpSend round-trip.
    let response =
        tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload_bytes))
            .await
            .expect("ShimManager::send did not return within 30s");

    let response_bytes = response.expect("ShimManager::send returned an error");

    // The response payload is the CBOR-encoded result from
    // fake-chromium's Page.navigate canned response: `{"frameId":
    // "fake-frame-1", "loaderId": "fake-loader-1"}`.
    let response_value: ciborium::value::Value =
        ciborium::de::from_reader(&response_bytes[..]).expect("response is valid CBOR");

    if let ciborium::value::Value::Map(entries) = &response_value {
        let has_frame_id = entries.iter().any(|(k, v)| {
            matches!(
                (k, v),
                (
                    ciborium::value::Value::Text(key),
                    ciborium::value::Value::Text(val),
                ) if key == "frameId" && val == "fake-frame-1"
            )
        });
        assert!(
            has_frame_id,
            "expected frameId='fake-frame-1' in response Map, got: {response_value:?}"
        );
    } else {
        panic!("expected Map response, got: {response_value:?}");
    }

    // Cleanup: shutdown_session should reap the chromium shim.
    mgr.shutdown_session("test-session-1").await;
    drop(user_data_dir);
}

/// Independent fitness signal for the web-verbs path — every
/// non-navigate verb's CDP envelope makes it through ShimManager →
/// loom-shim-chromium → fake-chromium and back as a CBOR Map. This
/// complements the unit tests in `loom-daemon/src/main.rs::tests`
/// (which assert envelope shape only) by validating the wire actually
/// decodes downstream.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn host_to_shim_to_fake_chromium_round_trip_per_verb() {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId("chromium:test-session-verbs".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    // Same shapes the daemon's `build_chromium_args` produces. The
    // unit test in loom-daemon owns the envelope-format invariant; this
    // test owns the wire-decodability invariant.
    let runtime_eval = |expr: &str| CdpMessage {
        method: "Runtime.evaluate".into(),
        params: ciborium::value::Value::Map(vec![
            (
                ciborium::value::Value::Text("expression".into()),
                ciborium::value::Value::Text(expr.into()),
            ),
            (
                ciborium::value::Value::Text("returnByValue".into()),
                ciborium::value::Value::Bool(true),
            ),
            (
                ciborium::value::Value::Text("awaitPromise".into()),
                ciborium::value::Value::Bool(false),
            ),
        ]),
    };

    let cases: Vec<CdpMessage> = vec![
        runtime_eval("document.querySelector(\"a\").click()"), // click
        runtime_eval("1+1"),                                   // evaluate
        runtime_eval(
            "(function(){const el=document.querySelector(\"input\");\
             el.value=\"hello\";\
             el.dispatchEvent(new Event('input',{bubbles:true}));\
             el.dispatchEvent(new Event('change',{bubbles:true}));})()",
        ), // type
        runtime_eval(
            "(function(){const el=document.querySelector(\"select\");\
             el.value=\"v1\";\
             el.dispatchEvent(new Event('change',{bubbles:true}));})()",
        ), // select
        runtime_eval(
            "document.querySelector(\"a\").dispatchEvent(\
             new MouseEvent('mouseover',{bubbles:true,cancelable:true}))",
        ), // hover
        runtime_eval(
            "(()=>{const el=\"body\"?document.querySelector(\"body\"):null;\
             const box=(!el||el===document.body||el===document.documentElement)\
             ?(document.scrollingElement||document.documentElement):el;\
             box.scrollBy(0,100);\
             return{x:window.scrollX,y:window.scrollY};})()",
        ), // scroll (viewport-targeting; see build_scroll_expression)
        runtime_eval("document.querySelector(\"a\") !== null"), // wait
        CdpMessage {
            method: "Page.captureScreenshot".into(),
            params: ciborium::value::Value::Map(vec![(
                ciborium::value::Value::Text("format".into()),
                ciborium::value::Value::Text("png".into()),
            )]),
        }, // screenshot
        CdpMessage {
            method: "DOM.getDocument".into(),
            params: ciborium::value::Value::Map(vec![
                (
                    ciborium::value::Value::Text("depth".into()),
                    ciborium::value::Value::Integer((-1i128).try_into().unwrap()),
                ),
                (
                    ciborium::value::Value::Text("pierce".into()),
                    ciborium::value::Value::Bool(true),
                ),
            ]),
        }, // snapshot
    ];

    for (idx, msg) in cases.iter().enumerate() {
        let payload = ciborium_to_vec(msg).expect("encode CdpMessage");
        let response = tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload))
            .await
            .unwrap_or_else(|_| panic!("verb #{idx} ({}) did not return in 30s", msg.method));

        let bytes =
            response.unwrap_or_else(|e| panic!("verb #{idx} ({}) errored: {e:?}", msg.method));

        let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..])
            .unwrap_or_else(|e| panic!("verb #{idx} ({}) response not CBOR: {e:?}", msg.method));

        assert!(
            matches!(value, ciborium::value::Value::Map(_)),
            "verb #{idx} ({}): expected Map, got {value:?}",
            msg.method
        );
    }

    mgr.shutdown_session("test-session-verbs").await;
    drop(user_data_dir);
}

/// pierce:true snapshot determinism — the pierced-path coverage that the
/// fake-chromium harness previously lacked (it ignored `pierce`). Drives a
/// `DOM.getDocument{depth:-1, pierce:true}` snapshot TWICE and asserts:
///   1. The pierced subtrees are actually inlined (shadowRoots + contentDocument).
///   2. The ephemeral per-capture frameIds are STRIPPED by normalization
///      (`loom_shared::dom_normalize` runs in the shim's `cdp_send`).
///   3. The two captures are byte-identical after normalization — node ids are
///      stable, only the frameIds varied, so a content-stable `dom_snapshot_hash`
///      holds across captures of the same pierced tree.
///
/// LIMIT: this validates the normalization PLUMBING for pierced subtrees only. It
/// does NOT reproduce real-Chromium node-id allocation or browser-enforced
/// same-origin / CORS isolation — the fixture uses stable synthetic node ids by
/// construction. Real-Chromium pierced node-id stability must be validated
/// separately (see specs/2026-06-09-unify-pierce-setting/plan.md).
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn host_to_shim_pierced_snapshot_normalizes_deterministically() {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId("chromium:test-session-pierce".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    // Same envelope the daemon's build_chromium_args(WebSnapshot) now produces.
    let snapshot = CdpMessage {
        method: "DOM.getDocument".into(),
        params: ciborium::value::Value::Map(vec![
            (
                ciborium::value::Value::Text("depth".into()),
                ciborium::value::Value::Integer((-1i128).try_into().unwrap()),
            ),
            (
                ciborium::value::Value::Text("pierce".into()),
                ciborium::value::Value::Bool(true),
            ),
        ]),
    };

    let payload1 = ciborium_to_vec(&snapshot).expect("encode CdpMessage");
    let bytes1 = tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload1))
        .await
        .expect("snapshot #1 did not return in 30s")
        .expect("snapshot #1 errored");

    let payload2 = ciborium_to_vec(&snapshot).expect("encode CdpMessage");
    let bytes2 = tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload2))
        .await
        .expect("snapshot #2 did not return in 30s")
        .expect("snapshot #2 errored");

    let s1 = String::from_utf8_lossy(&bytes1);
    assert!(
        s1.contains("shadowRoots"),
        "pierce:true must inline shadow-DOM subtrees"
    );
    assert!(
        s1.contains("contentDocument"),
        "pierce:true must inline iframe contentDocument subtrees"
    );
    assert!(
        !s1.contains("fake-frame"),
        "normalization must strip the ephemeral frameId from every pierced subtree"
    );
    assert_eq!(
        bytes1, bytes2,
        "two pierced captures must normalize to identical bytes (frameIds stripped, node ids stable)"
    );

    mgr.shutdown_session("test-session-pierce").await;
    drop(user_data_dir);
}

/// Parse PNG IHDR width/height (big-endian u32 at byte offsets 16 and 20).
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Some((w, h))
}

/// e2e (mcp-screenshot-delivery): a real shim + fake-chromium
/// `Page.captureScreenshot` round-trip, decoded through the SAME helper the
/// host/shim use before storing, must yield a valid raw PNG — proving the
/// content store will hold renderable bytes, not a CBOR{data:base64} envelope.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn screenshot_capture_decodes_to_valid_png() {
    use loom_shared::screenshot_decode::{decode_cdp_screenshot, is_png};

    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");

    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId("chromium:test-session-shot".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    let shot = CdpMessage {
        method: "Page.captureScreenshot".into(),
        params: ciborium::value::Value::Map(vec![(
            ciborium::value::Value::Text("format".into()),
            ciborium::value::Value::Text("png".into()),
        )]),
    };
    let payload = ciborium_to_vec(&shot).expect("encode CdpMessage");
    let response = tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload))
        .await
        .expect("screenshot did not return in 30s")
        .expect("screenshot errored");

    // The raw shim response is the CBOR `{data: base64}` envelope — NOT a PNG.
    assert!(
        !is_png(&response),
        "raw shim response must be the CBOR envelope, not a bare PNG"
    );

    // Decoding it (what the host/shim do before content_store.put) yields a
    // valid raw PNG with sane dimensions.
    let png = decode_cdp_screenshot(&response).expect("decode CDP screenshot to PNG");
    assert!(is_png(&png), "decoded bytes must start with PNG magic");
    assert!(png.len() >= 8, "decoded PNG must be non-trivial");
    let (w, h) = png_dimensions(&png).expect("decoded PNG must have an IHDR with dimensions");
    assert!(
        w >= 1 && h >= 1 && w <= 100_000 && h <= 100_000,
        "decoded PNG dimensions must be sane, got {w}x{h}"
    );

    mgr.shutdown_session("test-session-shot").await;
    drop(user_data_dir);
}

/// e2e (cdp-trusted-input): the trusted-input senders drive the real shim →
/// fake-chromium CDP path. Trusted click resolves the box-model center and
/// dispatches `Input.dispatchMouseEvent`; keystrokes / press_key dispatch real
/// `Input.dispatchKeyEvent`; a missing selector / unknown key map to typed
/// application outcomes (not transport errors).
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn trusted_input_dispatch_round_trip() {
    use loom_host::shim_manager::InputDispatchOutcome as O;
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!("fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first");
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    // Fixture: one clickable element with a known box model.
    let fixture_path = user_data_dir.path().join("fixture.json");
    std::fs::write(
        &fixture_path,
        r##"{"boxes":{"#submit":[10.0,20.0,110.0,60.0]},"viewport":[1280,720]}"##,
    )
    .expect("write fixture");

    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId("chromium:test-session-input".into());
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_FIXTURE".into(),
                    fixture_path.display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    // Trusted click on the fixtured element → Ok (box-model center resolved +
    // mouseMoved/Pressed/Released dispatched through the real shim).
    let click = tokio::time::timeout(
        Duration::from_secs(30),
        mgr.send_trusted_click(id.clone(), 0, 0, "#submit".into(), 0),
    )
    .await
    .expect("trusted click did not return in 30s")
    .expect("trusted click transport error");
    assert_eq!(
        click,
        O::Ok,
        "trusted click on a boxed element should succeed"
    );

    // Missing selector → SelectorNotFound (typed application outcome).
    let miss = mgr
        .send_trusted_click(id.clone(), 0, 0, "#missing".into(), 0)
        .await
        .expect("trusted click transport error");
    assert_eq!(miss, O::SelectorNotFound);

    // Real per-character keystrokes into the fixtured element → Ok.
    let typed = mgr
        .send_type_keystrokes(id.clone(), 0, 0, "#submit".into(), "hi".into(), 0)
        .await
        .expect("type_keystrokes transport error");
    assert_eq!(typed, O::Ok);

    // Ambient press_key (Enter, no selector) → Ok.
    let enter = mgr
        .send_press_key(loom_host::shim_manager::SendPressKeyParams {
            id: id.clone(),
            session_id: 0,
            target_id: 0,
            key: "Enter".into(),
            selector: None,
            modifiers: vec![],
            budget_ms: 0,
        })
        .await
        .expect("press_key transport error");
    assert_eq!(enter, O::Ok);

    // Unknown key → typed UnknownKey, NOT a transport error.
    let bad = mgr
        .send_press_key(loom_host::shim_manager::SendPressKeyParams {
            id: id.clone(),
            session_id: 0,
            target_id: 0,
            key: "NoSuchKey".into(),
            selector: None,
            modifiers: vec![],
            budget_ms: 0,
        })
        .await
        .expect("press_key transport error");
    assert_eq!(bad, O::UnknownKey);

    // web.type DEFAULT (fill / Input.insertText) into the fixtured element → Ok.
    // Mirrors the keystrokes path but commits the value via a single genuine
    // `Input.insertText` (Playwright `fill()` semantics) so React/RHF onChange fires.
    let filled = mgr
        .send_type_fill(
            id.clone(),
            0,
            0,
            "#submit".into(),
            "user@example.com".into(),
            0,
        )
        .await
        .expect("type_fill transport error");
    assert_eq!(
        filled,
        O::Ok,
        "fill into a resolvable element should succeed"
    );

    // Missing selector → SelectorNotFound (typed application outcome, not a transport error).
    let fill_miss = mgr
        .send_type_fill(id.clone(), 0, 0, "#missing".into(), "x".into(), 0)
        .await
        .expect("type_fill transport error");
    assert_eq!(fill_miss, O::SelectorNotFound);

    mgr.shutdown_session("test-session-input").await;
    drop(user_data_dir);
}

/// A shim over fake-chromium whose fixture scripts web.type fill's prepare
/// verdicts (`inputs`), logging every CDP message it receives.
fn fill_prepare_manager(
    tag: &str,
    fixture: &str,
) -> (
    std::sync::Arc<ShimManager>,
    ShimId,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    assert!(
        std::path::Path::new(&fake_path).exists(),
        "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
    );
    assert!(
        std::path::Path::new(&shim_path).exists(),
        "loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first"
    );
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let fixture_path = user_data_dir.path().join("fixture.json");
    std::fs::write(&fixture_path, fixture).expect("write fixture");
    let log_path = user_data_dir.path().join("cdp_methods.log");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId(format!("chromium:{tag}"));
    let dir = user_data_dir.path().display().to_string();
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_path),
                ("LOOM_SHIM_USER_DATA_DIR".into(), dir.clone()),
                ("LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(), dir),
                (
                    "LOOM_FAKE_CHROMIUM_FIXTURE".into(),
                    fixture_path.display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_LOG".into(),
                    log_path.display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 20,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );
    (mgr, id, user_data_dir, log_path)
}

/// The CDP log written after byte offset `from`, and the log's new length.
fn cdp_log_since(log_path: &std::path::Path, from: usize) -> (String, usize) {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    (log[from.min(log.len())..].to_string(), log.len())
}

/// e2e (web-type-fills-date-inputs): fill acts on the RESOLVED node. A
/// set-value input is filled by the prepare step alone (NO `Input.insertText`);
/// a rejected value / a disabled field are typed outcomes; any other target is
/// selected, then gets one `Input.insertText`. Every prepare failure is an `Err`
/// with a fixed message (no page text) — never a fall-through to insertText.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_prepare_routes_on_the_resolved_node() {
    use loom_host::shim_manager::{FillFailure, InputDispatchOutcome as O, SetValueType};
    let sels = [
        "#text",
        "#when",
        "#bad",
        "#locked",
        "#gone",
        "#throws",
        "#odd",
        "#oddtype",
        "#noobj",
        "#unresolvable",
    ];
    let boxes = sels
        .iter()
        .map(|s| format!("\"{s}\":[10.0,20.0,110.0,60.0]"))
        .collect::<Vec<_>>()
        .join(",");
    let fixture = format!(
        r##"{{"boxes":{{{boxes}}},"inputs":{{"#when":"set","#bad":"malformed:date","#locked":"not_editable","#gone":"detached","#throws":"throw","#odd":"garbage","#oddtype":"malformed:text","#noobj":"no_object","#unresolvable":"resolve_error"}}}}"##
    );
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-prepare-routes", &fixture);
    let fill = |sel: &'static str| {
        let fut = mgr.send_type_fill(id.clone(), 0, 0, sel.into(), "2036-12-31".into(), 0);
        async move {
            tokio::time::timeout(Duration::from_secs(30), fut)
                .await
                .expect("fill did not return in 30s")
        }
    };

    // A plain editable: prepared (selected) on the resolved node, then ONE insertText.
    let (_, mut at) = cdp_log_since(&log_path, 0);
    assert_eq!(fill("#text").await.expect("fill transport error"), O::Ok);
    let (sent, next) = cdp_log_since(&log_path, at);
    at = next;
    for method in [
        "DOM.resolveNode",
        "Runtime.callFunctionOn",
        "Runtime.releaseObjectGroup",
        "Input.insertText",
    ] {
        assert!(sent.contains(method), "{method} missing; CDP log:\n{sent}");
    }
    assert!(
        !sent.contains("Runtime.evaluate"),
        "fill must not re-query the raw selector in a page evaluate; CDP log:\n{sent}"
    );

    // Set-value input: done by the prepare step — no insertText at all.
    assert_eq!(fill("#when").await.expect("fill transport error"), O::Ok);
    let (sent, next) = cdp_log_since(&log_path, at);
    at = next;
    assert!(sent.contains("Runtime.callFunctionOn"), "CDP log:\n{sent}");
    assert!(
        !sent.contains("Input.insertText"),
        "a set-value input must not also get insertText; CDP log:\n{sent}"
    );

    // Typed refusals, still without insertText.
    assert_eq!(
        fill("#bad").await.expect("fill transport error"),
        O::MalformedValue(SetValueType::Date)
    );
    assert_eq!(
        fill("#locked").await.expect("fill transport error"),
        O::NotEditable
    );
    let (sent, next) = cdp_log_since(&log_path, at);
    at = next;
    assert!(!sent.contains("Input.insertText"), "CDP log:\n{sent}");

    // A page-side reason the element cannot be filled is a TYPED outcome with a
    // fixed message (no page text), and never types.
    for (sel, failure) in [
        ("#gone", FillFailure::Detached),
        ("#throws", FillFailure::PageException),
        ("#odd", FillFailure::UnrecognisedVerdict),
        ("#oddtype", FillFailure::UnrecognisedVerdict),
        ("#noobj", FillFailure::NoObject),
        ("#unresolvable", FillFailure::Rejected),
    ] {
        let outcome = fill(sel).await.expect("fill transport error");
        assert_eq!(outcome, O::FillFailed(failure), "{sel}");
        assert!(
            !failure.message().contains("fake page exception"),
            "{sel}: page text leaked into the message"
        );
    }
    let (sent, _) = cdp_log_since(&log_path, at);
    assert!(
        !sent.contains("Input.insertText"),
        "a failed prepare must never fall through to insertText; CDP log:\n{sent}"
    );

    // Page-side failures are the page's doing, not the shim's: a run of them must
    // not trip the circuit breaker (threshold 20 here) and lock the session out.
    for _ in 0..25 {
        assert_eq!(
            fill("#gone").await.expect("fill transport error"),
            O::FillFailed(FillFailure::Detached)
        );
    }
    assert_eq!(
        fill("#text")
            .await
            .expect("the breaker must stay closed after page-side failures"),
        O::Ok
    );

    // Every fill releases only its OWN object group: no two fills share one.
    let (whole, _) = cdp_log_since(&log_path, 0);
    let groups: Vec<&str> = whole
        .lines()
        .filter(|l| l.contains("DOM.resolveNode"))
        .filter_map(|l| l.split("\"objectGroup\":\"").nth(1))
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert!(
        groups.len() >= 8,
        "expected a resolveNode per fill; got {groups:?}"
    );
    let unique: std::collections::HashSet<&&str> = groups.iter().collect();
    assert_eq!(
        unique.len(),
        groups.len(),
        "object groups reused: {groups:?}"
    );

    mgr.shutdown_session("fill-prepare-routes").await;
}

/// e2e: a lost ack in fill's prepare step is a BOUNDED error — the action's
/// budget, not the 30 s recv floor — and never a success: the text field was
/// never typed into, and a set-value input's state is unknown.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_prepare_lost_ack_is_a_bounded_error() {
    let fixture = r##"{"boxes":{"#lost-resolve":[10.0,20.0,110.0,60.0],"#lost-call":[10.0,80.0,110.0,120.0]},"inputs":{"#lost-resolve":"swallow_resolve","#lost-call":"swallow_call"}}"##;
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-lost-ack", fixture);
    for sel in ["#lost-resolve", "#lost-call"] {
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(20),
            mgr.send_type_fill(id.clone(), 0, 0, sel.into(), "2036-12-31".into(), 800),
        )
        .await
        .unwrap_or_else(|_| panic!("{sel}: a lost prepare ack must not dead-wait"))
        .expect_err(&format!(
            "{sel}: a lost prepare ack must be an error, not a success"
        ));
        let elapsed = started.elapsed();
        assert!(err.to_string().contains("unacknowledged"), "{sel}: {err}");
        assert!(
            elapsed < Duration::from_secs(10),
            "{sel}: bounded by the 800 ms budget, not the 30 s floor; took {elapsed:?}"
        );
    }
    let (sent, _) = cdp_log_since(&log_path, 0);
    assert!(
        !sent.contains("Input.insertText"),
        "a lost prepare ack must never fall through to insertText; CDP log:\n{sent}"
    );
    mgr.shutdown_session("fill-lost-ack").await;
}

/// e2e: once the action's budget is spent, fill sends nothing more that could
/// change the page — no prepare call, no insertText — and says it ran out of time.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_with_a_spent_deadline_writes_nothing() {
    let fixture = r##"{"boxes":{"#text":[10.0,20.0,110.0,60.0],"#when":[10.0,80.0,110.0,120.0]},"inputs":{"#when":"set"}}"##;
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-spent-deadline", fixture);
    for sel in ["#text", "#when"] {
        // 1 ms: selector resolution alone (several shim round-trips) spends it.
        let err = mgr
            .send_type_fill(id.clone(), 0, 0, sel.into(), "2036-12-31".into(), 1)
            .await
            .expect_err(&format!("{sel}: a spent deadline must be an error"));
        assert!(err.to_string().contains("deadline ran out"), "{sel}: {err}");
    }
    let (sent, _) = cdp_log_since(&log_path, 0);
    for write in ["Runtime.callFunctionOn", "Input.insertText"] {
        assert!(
            !sent.contains(write),
            "{write} sent after the deadline was spent; CDP log:\n{sent}"
        );
    }
    mgr.shutdown_session("fill-spent-deadline").await;
}

/// e2e: releasing a fill's CDP object group is best-effort — a failing release
/// never turns a committed fill into an error.
#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn fill_release_failure_does_not_fail_a_committed_fill() {
    use loom_host::shim_manager::InputDispatchOutcome as O;
    let fixture = r##"{"boxes":{"#when":[10.0,20.0,110.0,60.0],"#text":[10.0,80.0,110.0,120.0]},"inputs":{"#when":"set"},"release_error":true}"##;
    let (mgr, id, _udd, log_path) = fill_prepare_manager("fill-release-error", fixture);
    for sel in ["#when", "#text"] {
        let outcome = mgr
            .send_type_fill(id.clone(), 0, 0, sel.into(), "18:00".into(), 0)
            .await
            .unwrap_or_else(|e| panic!("{sel}: a release failure must not fail the fill: {e}"));
        assert_eq!(outcome, O::Ok, "{sel}");
    }
    let (sent, _) = cdp_log_since(&log_path, 0);
    assert!(
        sent.contains("Runtime.releaseObjectGroup"),
        "CDP log:\n{sent}"
    );
    mgr.shutdown_session("fill-release-error").await;
}

/// Interaction-fingerprint (capture-policy=fingerprint) MECHANISM e2e.
///
/// Reproduces exactly what the host `capture_dom_after_hash` fn does — issue
/// `DOM.getDocument {depth:-1, pierce:true}` via `ShimManager::send` and sha256
/// the shim-normalized response — against an extended fake-chromium whose DOM
/// varies by a prior DOM-mutating "click". Proves the two properties the
/// per-verb-constant `outcome_hash` cannot provide.
async fn dom_after_hash_via_shim(label: &str, mutate: bool) -> String {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!("loom-shim-chromium binary not built at {shim_path}; run `cargo build -p loom-cli --bin loom-shim-chromium` first");
    }
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let mgr = ShimManager::new(HostObservability::new(true));
    let id = ShimId(format!("chromium:{label}"));
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_path.into(),
            args: vec![],
            env: vec![
                ("LOOM_SHIM_CHROMIUM_PATH".into(), fake_chromium_bin()),
                (
                    "LOOM_SHIM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
                (
                    "LOOM_FAKE_CHROMIUM_USER_DATA_DIR".into(),
                    user_data_dir.path().display().to_string(),
                ),
            ],
            spawn_retry: 1,
            breaker_threshold: 3,
            breaker_open_ms: 5_000,
            send_timeout_ms: 10_000,
            recv_timeout_ms: 30_000,
        },
    );

    if mutate {
        // Model a DOM-mutating click: the fake flips per-connection state so the
        // SUBSEQUENT DOM.getDocument returns content-differing DOM.
        let click = CdpMessage {
            method: "Runtime.evaluate".into(),
            params: ciborium::value::Value::Map(vec![(
                ciborium::value::Value::Text("expression".into()),
                ciborium::value::Value::Text("__loom_test_dom_mutate__".into()),
            )]),
        };
        let payload = ciborium_to_vec(&click).expect("encode click");
        tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload))
            .await
            .expect("click did not return in 30s")
            .expect("click errored");
    }

    // The exact envelope `capture_dom_after_hash` issues.
    let dom = CdpMessage {
        method: "DOM.getDocument".into(),
        params: ciborium::value::Value::Map(vec![
            (
                ciborium::value::Value::Text("depth".into()),
                ciborium::value::Value::Integer((-1i64).into()),
            ),
            (
                ciborium::value::Value::Text("pierce".into()),
                ciborium::value::Value::Bool(true),
            ),
        ]),
    };
    let payload = ciborium_to_vec(&dom).expect("encode DOM.getDocument");
    let resp = tokio::time::timeout(Duration::from_secs(30), mgr.send(id.clone(), payload))
        .await
        .expect("DOM.getDocument did not return in 30s")
        .expect("DOM.getDocument errored");

    mgr.shutdown_session(label).await;
    drop(user_data_dir);
    // Same hash `capture_dom_after_hash` computes (sha256 of the normalized
    // DOM.getDocument response).
    loom_core::content_store::sha256_hex(&resp)
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; run `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium` first"]
async fn interaction_dom_after_hash_is_deterministic_and_content_bearing() {
    // Two independent same-shape "fingerprint" sessions that both perform the
    // mutating interaction must produce the SAME dom_after_hash — determinism:
    // each fake subprocess emits DIFFERENT ephemeral frameIds, which the shim's
    // normalize seam strips, so the content-derived hash matches.
    let h_mut_a = dom_after_hash_via_shim("fp-mut-a", true).await;
    let h_mut_b = dom_after_hash_via_shim("fp-mut-b", true).await;
    // A no-op interaction (no DOM mutation) must produce a DIFFERENT hash —
    // proving the fingerprint is content-bearing, unlike the constant outcome_hash.
    let h_noop = dom_after_hash_via_shim("fp-noop", false).await;

    assert_eq!(h_mut_a.len(), 64, "dom_after_hash must be 64 hex chars");
    assert!(
        h_mut_a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "dom_after_hash must be lowercase hex"
    );
    assert_eq!(
        h_mut_a, h_mut_b,
        "two same-shape mutating sessions must yield an identical dom_after_hash (determinism)"
    );
    assert_ne!(
        h_mut_a, h_noop,
        "a DOM-mutating interaction must yield a different dom_after_hash than a no-op (content-bearing)"
    );
}

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
