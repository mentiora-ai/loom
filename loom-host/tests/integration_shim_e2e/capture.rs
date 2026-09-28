//! What the shim captures from a page: screenshots and the interaction dom_after_hash.

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{ShimConfig, ShimId, ShimManager};
use loom_shared::shim_protocol::{ciborium_to_vec, CdpMessage};
use std::time::Duration;

use crate::common::*;

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
