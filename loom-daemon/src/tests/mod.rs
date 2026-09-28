//! Unit tests for the daemon crate (split out of `lib.rs`): each submodule
//! covers one area; shared imports and helpers live here.

use super::*;
use loom_shared::shim_protocol::CdpMessage;
// Names the tests use that, post-split, no longer flow through the
// non-test `lib.rs` `use` block (their former non-test users — the
// `CoreBridge` / `WasmBridge` impls — moved to sibling submodules).
// `CoreFacadeBridge` is imported for trait-method resolution on the
// `CoreBridge` close/abort/create tests.
use loom_core::receipt_builder::receipt_builder::NetworkSummary;
use loom_host::receipt_marshaller::{ReceiptBuilder, ReceiptStatus as HostStatus};
use loom_rpc::core_service_adapter::core_service_adapter::{CoreFacadeBridge, CreateSessionParams};
use loom_rpc::host_service_adapter::host_service_adapter::{Action, Receipt};
use loom_shared::navigate_outcome::{LoomNetworkEvent, ShimConsoleLine};
use std::path::PathBuf;

/// Decode `build_chromium_args` output into the wire-shape struct.
/// Returns None if the function returned None (legacy fallback path).
fn decode_cdp(action: &Action) -> Option<CdpMessage> {
    let bytes = build_chromium_args(action)?;
    Some(ciborium::de::from_reader::<CdpMessage, _>(bytes.as_slice()).expect("valid CdpMessage"))
}
fn s(v: &str) -> String {
    v.to_string()
}
fn params_get<'a>(msg: &'a CdpMessage, key: &str) -> Option<&'a ciborium::value::Value> {
    match &msg.params {
        ciborium::value::Value::Map(entries) => entries.iter().find_map(|(k, v)| match k {
            ciborium::value::Value::Text(t) if t == key => Some(v),
            _ => None,
        }),
        _ => None,
    }
}
fn expr_of(msg: &CdpMessage) -> &str {
    match params_get(msg, "expression").expect("expression param") {
        ciborium::value::Value::Text(t) => t.as_str(),
        _ => panic!("expression not a Text"),
    }
}
fn nav_event(status: u16, bytes: u64) -> LoomNetworkEvent {
    LoomNetworkEvent {
        method: "GET".into(),
        url: "https://example.com/x".into(),
        request_hash: "0".repeat(64),
        response_hash: "1".repeat(64),
        status,
        content_type: "text/html".into(),
        duration_ms: 50,
        response_bytes: bytes,
        error_reason: None,
        error_kind: None,
    }
}
fn navigate_builder_with_all_blobs() -> ReceiptBuilder {
    let console_lines = vec![ShimConsoleLine {
        level: "info".into(),
        message: "ready".into(),
    }];
    let summary = NetworkSummary {
        total_count: 2,
        total_bytes: 5120,
        error_count: 0,
    };
    let events = vec![nav_event(200, 4096), nav_event(200, 1024)];
    ReceiptBuilder {
        action_id: 11,
        finished_at_ms: 250,
        started_at_ms: 0,
        status: HostStatus::Ok,
        action_hash: "aa".repeat(32),
        outcome_hash: "bb".repeat(32),
        emitted_at_ms: 1_714_074_336_000,
        navigate_url: Some("https://example.com/".into()),
        navigate_final_url: Some("https://example.com/".into()),
        navigate_title: Some("Example".into()),
        navigate_status_code: Some(200),
        navigate_dom_snapshot_hash: Some("a".repeat(64)),
        navigate_screenshot_after_hash: Some("b".repeat(64)),
        navigate_console_count: Some(1),
        navigate_network_count: Some(2),
        navigate_side_effects_json: Some(serde_json::to_vec(&events).unwrap()),
        navigate_console_lines_json: Some(serde_json::to_vec(&console_lines).unwrap()),
        navigate_network_summary_json: Some(serde_json::to_vec(&summary).unwrap()),
        ..Default::default()
    }
}
/// Scratch dir under the test TMPDIR, keyed by test name + pid so it stays
/// unique under parallel execution (and across concurrent test processes).
fn test_scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-daemon-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test scratch dir");
    dir
}
fn make_core_at(data_root: &std::path::Path) -> Arc<CoreApiFacade> {
    let config = CoreConfig {
        data_root: data_root.to_path_buf(),
        log_path: data_root.join("daemon.log"),
        otel_enabled: false,
        default_seed: 42,
        checkpoint_every_n: 100,
    };
    let keychain: Arc<dyn loom_core::vault::KeychainAccess> = Arc::new(loom_keychain::StubKeychain);
    CoreApiFacade::new(config, keychain).expect("CoreApiFacade::new in scratch dir")
}
/// A CoreBridge with a REAL WasmHost (empty surfaces dir — no modules,
/// no chromium template) so `spawn_shim_teardown` takes the Some(host)
/// path exactly like a production daemon.
fn make_bridge(data_root: &std::path::Path) -> CoreBridge {
    let core = make_core_at(data_root);
    let host = loom_host::WasmHost::new(
        Arc::clone(&core),
        loom_host::HostConfig {
            surfaces_dir: data_root.join("surfaces-empty"),
            shim_chromium: None,
            ..Default::default()
        },
    )
    .expect("WasmHost::new with empty surfaces dir");
    CoreBridge {
        core,
        wasm_host: Some(host),
        cleanup_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
    }
}
/// Default test session params (profile "safe", all options off) — the struct
/// equivalent of the old positional `"safe","isolated",None,None,None,false,false,None,false`.
fn params_safe() -> CreateSessionParams {
    CreateSessionParams {
        profile: "safe".to_string(),
        network_mode: "isolated".to_string(),
        capture_policy: None,
        seed: None,
        budget: None,
        no_blocklist: false,
        no_determinism: false,
        clock_anchor: None,
        record_screencast: false,
        audio: false,
    }
}
fn create_session_via(bridge: &CoreBridge) -> String {
    let (sid, _) = bridge
        .create_session_raw(params_safe())
        .expect("create_session_raw");
    sid
}
/// Run `f` with the loom env vars parse_args reads pinned to a known
/// state (LOOM_LOG_PATH optionally set, the rest cleared), restoring
/// the previous values afterwards. Holds `ENV_LOCK` across the whole
/// mutate→read→restore window so concurrent env-mutating tests (cargo's
/// default parallelism) can't observe each other's transient env state.
fn with_parse_args_env<T>(log_path: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    const KEYS: &[&str] = &[
        "LOOM_SOCKET_PATH",
        "LOOM_DATA_ROOT",
        "LOOM_LOG_PATH",
        "LOOM_OTEL_ENABLED",
        "LOOM_UPLOAD_ROOT",
    ];
    let saved: Vec<(&str, Option<String>)> =
        KEYS.iter().map(|k| (*k, std::env::var(*k).ok())).collect();
    for k in KEYS {
        std::env::remove_var(k);
    }
    if let Some(v) = log_path {
        std::env::set_var("LOOM_LOG_PATH", v);
    }
    let out = f();
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    out
}
fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}
/// Run `f` with the process umask temporarily set to `mask` (tests run
/// --test-threads=1, so no other thread races the process-global umask).
#[cfg(unix)]
fn with_umask<T>(mask: libc::mode_t, f: impl FnOnce() -> T) -> T {
    let old = unsafe { libc::umask(mask) };
    let out = f();
    unsafe { libc::umask(old) };
    out
}

/// Serializes tests that mutate process-global env (`std::env::set_var`/`remove_var`).
/// Env is per-process, so under parallel test execution (cargo's default, or
/// `cargo test` without `--test-threads=1`) two such tests clobber each other. Every
/// env-mutating test in this module acquires this lock for the duration of its
/// mutate→read→restore window. (nextest runs each test in its own process and is
/// immune regardless; this keeps plain `cargo test` solid too.)
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

mod config;
mod gates;
mod input_receipts;
mod lifecycle;
mod navigate_receipt;
mod payloads;
