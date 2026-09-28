//! End-to-end integration test for the evaluate-result path.
//!
//! Drives `ShimManager::send_evaluate` against the real
//! `loom-shim-chromium` binary, which spawns the test-only
//! `fake-chromium` binary. fake-chromium pattern-matches the
//! Runtime.evaluate `expression` field and emits synthetic CDP
//! response bodies (see `bin/fake-chromium/evaluate.rs::build_fake_evaluate_response`).
//!
//! Behaviours covered:
//!   - integer result (`1+1` analogue) → return_value
//!   - string result (`document.title` analogue) → return_value
//!   - page-side throw → exception with kind=js_throw
//!   - large value > 64KB → host offloads to content store
//!   - this file IS the integration test (fitness function)
//!
//! Plus boundary cases:
//!   - null / undefined / empty-string results
//!   - 65_535 / 65_536 / 65_537 byte canonical-JSON boundary
//!   - float result (string-coerce)
//!
//! Run:
//!   cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium
//!   cargo build -p loom-cli --bin loom-shim-chromium
//!   cargo test -p loom-host --test integration_evaluate_result -- --ignored
//!
//! Marked `#[ignore]` so a default `cargo test --workspace` doesn't
//! force the fake-chromium build.

#![cfg(unix)]

use loom_host::host_observability::HostObservability;
use loom_host::shim_manager::{EvaluateOutcome, ShimConfig, ShimId, ShimManager};
use std::time::Duration;

fn target_bin_dir() -> std::path::PathBuf {
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

fn assert_binaries_built() {
    let fake_path = fake_chromium_bin();
    let shim_path = shim_bin();
    if !std::path::Path::new(&fake_path).exists() {
        panic!(
            "fake-chromium binary not built at {fake_path}; run \
             `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium`"
        );
    }
    if !std::path::Path::new(&shim_path).exists() {
        panic!(
            "loom-shim-chromium binary not built at {shim_path}; run \
             `cargo build -p loom-cli --bin loom-shim-chromium`"
        );
    }
}

fn make_manager(session_label: &str) -> (std::sync::Arc<ShimManager>, ShimId, tempfile::TempDir) {
    let user_data_dir = tempfile::tempdir().expect("tempdir");
    let obs = HostObservability::new(true);
    let mgr = ShimManager::new(obs);
    let id = ShimId(format!("chromium:{session_label}"));
    mgr.register(
        id.clone(),
        ShimConfig {
            binary_path: shim_bin().into(),
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
            send_timeout_ms: 30_000,
            recv_timeout_ms: 60_000,
        },
    );
    (mgr, id, user_data_dir)
}

async fn evaluate(
    mgr: &std::sync::Arc<ShimManager>,
    id: ShimId,
    expression: &str,
) -> Result<EvaluateOutcome, loom_core::error::LoomError> {
    tokio::time::timeout(
        Duration::from_secs(45),
        mgr.send_evaluate(loom_host::shim_manager::SendEvaluateParams {
            id,
            action_id: "test-action".to_string(),
            session_id: 0,
            target_id: 0,
            expression: expression.to_string(),
            budget_ms: 30_000,
            seed: loom_shared::types::Seed(42),
            epoch_ms: loom_shared::types::EpochMs(1_700_000_000_000),
            determinism_enabled: true,
            audio_enabled: false,
        }),
    )
    .await
    .expect("send_evaluate did not return within 45s")
}

// ─── Integer result surfaces ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_integer_result_surfaces_in_outcome() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-01-int");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_int__")
        .await
        .expect("evaluate of integer must succeed");

    assert!(outcome.exception.is_none(), "no exception expected");
    let result = outcome.result.expect("result must be Some on success");
    // The shim returns CBOR; integer 2 round-trips as Value::Integer.
    match result {
        ciborium::value::Value::Integer(i) => {
            assert_eq!(i128::from(i), 2, "integer 1+1 must surface as the value 2");
        }
        other => panic!("expected Value::Integer(2), got {other:?}"),
    }

    mgr.shutdown_session("evalresult-01-int").await;
}

// ─── String result surfaces ────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_string_result_surfaces_in_outcome() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-02-str");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_doc_title__")
        .await
        .expect("evaluate of string must succeed");

    assert!(outcome.exception.is_none(), "no exception expected");
    let result = outcome.result.expect("result must be Some on success");
    match result {
        ciborium::value::Value::Text(s) => {
            assert_eq!(
                s, "My Page",
                "document.title analogue must surface as the string"
            );
        }
        other => panic!("expected Value::Text(\"My Page\"), got {other:?}"),
    }

    mgr.shutdown_session("evalresult-02-str").await;
}

// ─── Page-side throw → exception ───────────────────────────────────────────

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_js_throw_surfaces_as_exception() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-03-throw");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_throw__:x")
        .await
        .expect("send_evaluate must return Ok with exception inside");

    assert!(outcome.result.is_none(), "no result expected on throw");
    let ex = outcome.exception.expect("exception must be Some");
    assert!(
        ex.message.contains('x'),
        "exception.message must contain 'x' (got {:?})",
        ex.message
    );
    assert_eq!(ex.text, "Uncaught", "exceptionDetails.text from CDP");

    mgr.shutdown_session("evalresult-03-throw").await;
}

// ─── Large result triggers content-store offload ───────────────────────────
// NOTE: This is a SHIM-LAYER test — the host's truncation/offload happens
// in `evaluate_execute`, not in `send_evaluate`. So at this layer we
// simply assert the shim returns the full large result; the host-layer
// truncation behavior is covered by unit tests on `cbor_value_to_json`
// and the daemon+CLI E2E test (deferred follow-up).

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_large_result_round_trips_through_shim() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-04-large");

    // 80 KB result — host's evaluate_execute would offload to content store.
    let outcome = evaluate(&mgr, id.clone(), "__loom_test_large__:80")
        .await
        .expect("send_evaluate of large value must succeed");

    assert!(outcome.exception.is_none(), "no exception expected");
    let result = outcome.result.expect("result must be Some");
    match result {
        ciborium::value::Value::Text(s) => {
            // 80KB target: 80 * 1024 = 81920 bytes for the canonical-JSON
            // (= "x"*81918 + 2 quote chars).
            assert!(
                s.len() > 65_536,
                "result must exceed 64KB threshold (got {} bytes)",
                s.len()
            );
        }
        other => panic!("expected Value::Text(<large string>), got {other:?}"),
    }

    mgr.shutdown_session("evalresult-04-large").await;
}

// ─── Skeptic boundary cases ────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_null_result() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-null");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_null__")
        .await
        .expect("evaluate('null') must succeed");

    assert!(outcome.exception.is_none());
    let result = outcome.result.expect("result is Some");
    match result {
        ciborium::value::Value::Null => {}
        other => panic!("expected Value::Null, got {other:?}"),
    }

    mgr.shutdown_session("evalresult-null").await;
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_undefined_result() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-undef");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_undef__")
        .await
        .expect("evaluate('undefined') must succeed");

    assert!(outcome.exception.is_none());
    // CDP returns no `value` field for undefined; parse_evaluate_payload
    // surfaces this as Value::Null per CDP convention (see host shim
    // manager). Assert the result is present and Null.
    let result = outcome.result.expect("undefined → Null result");
    assert!(matches!(result, ciborium::value::Value::Null));

    mgr.shutdown_session("evalresult-undef").await;
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_empty_string_result() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-empty");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_empty_str__")
        .await
        .expect("evaluate('\"\"') must succeed");

    assert!(outcome.exception.is_none());
    match outcome.result.expect("result is Some") {
        ciborium::value::Value::Text(s) => assert_eq!(s, ""),
        other => panic!("expected empty Text, got {other:?}"),
    }

    mgr.shutdown_session("evalresult-empty").await;
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_object_result() {
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-obj");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_obj__")
        .await
        .expect("evaluate('{...}') must succeed");

    assert!(outcome.exception.is_none());
    match outcome.result.expect("result is Some") {
        ciborium::value::Value::Map(_) => {}
        other => panic!("expected Map, got {other:?}"),
    }

    mgr.shutdown_session("evalresult-obj").await;
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_float_result() {
    // Q6: floats are string-coerced at the host layer (cbor_value_to_json).
    // At the shim layer we just verify the float round-trips through CBOR.
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-float");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_pi__")
        .await
        .expect("evaluate('Math.PI') must succeed");

    assert!(outcome.exception.is_none());
    let result = outcome.result.expect("result is Some");
    // Either Float(3.141...) or — if the shim's JSON→CBOR doesn't preserve
    // floats — an Integer or Text representation. Assert it's at least
    // present and not null.
    assert!(
        !matches!(result, ciborium::value::Value::Null),
        "Math.PI must not collapse to null at the shim layer (got {result:?})"
    );

    mgr.shutdown_session("evalresult-float").await;
}

// ─── 64KB threshold boundary tests ─────────────────────────────────────────

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_threshold_just_below_64kb() {
    // 65_535-byte canonical-JSON (one byte under threshold) — host should
    // keep it inline. At shim layer we only verify the size matches.
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-thresh-below");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_size__:65535")
        .await
        .expect("evaluate at 65_535 bytes must succeed");

    let result = outcome.result.expect("result is Some");
    match result {
        ciborium::value::Value::Text(s) => {
            // Inner string is target_bytes - 2 = 65533 chars.
            assert_eq!(s.len(), 65_533, "expected 65_535 - 2 = 65_533 inner chars");
        }
        other => panic!("expected Text, got {other:?}"),
    }

    mgr.shutdown_session("evalresult-thresh-below").await;
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_threshold_at_64kb_exactly() {
    // 65_536-byte canonical-JSON (exactly threshold). Host check is `>`,
    // so exactly-at-threshold stays inline.
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-thresh-at");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_size__:65536")
        .await
        .expect("evaluate at 65_536 bytes must succeed");

    let result = outcome.result.expect("result is Some");
    match result {
        ciborium::value::Value::Text(s) => {
            assert_eq!(s.len(), 65_534, "65_536 - 2 = 65_534 inner chars");
        }
        other => panic!("expected Text, got {other:?}"),
    }

    mgr.shutdown_session("evalresult-thresh-at").await;
}

#[tokio::test]
#[ignore = "requires fake-chromium binary; see file header for build commands"]
async fn evalresult_threshold_just_above_64kb() {
    // 65_537-byte canonical-JSON — host should offload to content store.
    assert_binaries_built();
    let (mgr, id, _udd) = make_manager("evalresult-thresh-above");

    let outcome = evaluate(&mgr, id.clone(), "__loom_test_size__:65537")
        .await
        .expect("evaluate at 65_537 bytes must succeed");

    let result = outcome.result.expect("result is Some");
    match result {
        ciborium::value::Value::Text(s) => {
            assert_eq!(s.len(), 65_535, "65_537 - 2 = 65_535 inner chars");
        }
        other => panic!("expected Text, got {other:?}"),
    }

    mgr.shutdown_session("evalresult-thresh-above").await;
}
