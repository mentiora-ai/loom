//! The basic host→shim→fake-chromium round trips: one CDP call, every verb, the pierced snapshot.

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use loom_shared::shim_protocol::{ciborium_to_vec, CdpMessage};
use std::time::Duration;

use crate::common::*;

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
