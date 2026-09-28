//! End-to-end test for the README 5-minute-quickstart block (task 04, AC3 / R3).
//!
//! Two layers, mirroring the rest of the loom-cli integration suite:
//!
//! - **`quickstart_readme_uses_idiomatic_single_session_form`** — a fast,
//!   no-spawn guard that reads `README.md` and asserts the quickstart's
//!   `web.navigate` / `web.evaluate` lines use the idiomatic single-`--session`
//!   form. This locks AC3's "uses the idiomatic single-`--session` form" against
//!   regression of the README text itself (the bug this task fixes was a
//!   duplicated `--session $SESSION -- --session $SESSION`). Runs on every
//!   `cargo test`.
//!
//! - **`quickstart_block_executes_end_to_end`** — the AC3 "executes the
//!   quickstart block end-to-end against a running daemon and asserts a
//!   successful receipt" test. It drives the quickstart's command sequence
//!   (session create → web.navigate → web.evaluate → session close) through the
//!   real `loom` CLI against a real `loom-daemon`, via the shared
//!   [`DaemonTestHarness`] (task 01). Like the other daemon-backed e2e tests it
//!   is `#[ignore]`d and runs in CI under `--include-ignored`, with 0 retries
//!   and a single bounded ready-wait (FND-0006 no-flake bar).
//!
//! ## Why the navigate target is `http://fake.test/...`, not the README's
//! ## `https://example.com`
//!
//! The README quickstart navigates to `https://example.com` — a real network
//! fetch, which a hermetic CI test must NOT perform (flaky, network-gated).
//! `fake-chromium` (the test CDP endpoint) intercepts ALL navigation and
//! returns scripted responses keyed by URL path: `http://fake.test/status/200`
//! deterministically yields a 200 success receipt (see
//! `loom-shims/src/bin/fake-chromium/`). So this test exercises the exact
//! command *form* the README documents — proving the single-`--session`
//! invocation parses and round-trips through the daemon — while staying
//! hermetic. The README-text form itself is locked by the no-spawn test above.
//!
//! ## Build prereqs (same family as `integration_naverr_cli_e2e`)
//!
//! `cargo test -p loom-cli` builds the `loom`, `loom-daemon`, and
//! `loom-shim-chromium` bins automatically (they live in this package). Two
//! extra artifacts must be present first:
//!
//! ```sh
//! cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium
//! cargo build --target wasm32-wasip2 -p loom-surface-web --release
//! cargo test -p loom-cli --test integration_quickstart_snippet -- --include-ignored
//! ```
//!
//! The `loom_surface_web.wasm` artifact is AOT-compiled to a `.cwasm` and
//! symlinked into the harness's hermetic `~/.config/loom/surfaces/`; chromium is
//! wired to `fake-chromium` via `LOOM_CHROMIUM_PATH`.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::daemon_test_harness::DaemonTestHarness;

// ─── Layer 1: no-spawn README-form guard (runs on every `cargo test`) ───────

#[test]
fn quickstart_readme_uses_idiomatic_single_session_form() {
    let readme = std::fs::read_to_string(workspace_root().join("README.md"))
        .expect("read README.md at workspace root");

    let nav = quickstart_line(&readme, "loom action web.navigate");
    let eval = quickstart_line(&readme, "loom action web.evaluate");

    // Idiomatic form: `--session` is a named clap arg; the remaining
    // `--key value` pairs are `trailing_var_arg`. No `--` separator, and
    // `--session` appears exactly once.
    for (label, line) in [("web.navigate", &nav), ("web.evaluate", &eval)] {
        assert!(
            !line.contains(" -- "),
            "quickstart {label} line must not use a `--` arg separator \
             (the pre-fix bug); got: {line}"
        );
        assert_eq!(
            line.matches("--session").count(),
            1,
            "quickstart {label} line must name `--session` exactly once \
             (the pre-fix bug duplicated it); got: {line}"
        );
    }

    assert!(
        nav.contains("--session $SESSION --url "),
        "web.navigate must read `--session $SESSION --url <URL>`; got: {nav}"
    );
    assert!(
        eval.contains("--session $SESSION --expression "),
        "web.evaluate must read `--session $SESSION --expression <EXPR>`; got: {eval}"
    );
}

/// Return the trimmed quickstart line beginning with `needle`. Panics with a
/// clear message if absent, so a future README edit that drops the line fails
/// loudly rather than silently passing.
fn quickstart_line(readme: &str, needle: &str) -> String {
    readme
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(needle))
        .unwrap_or_else(|| panic!("README quickstart is missing a line starting with `{needle}`"))
        .to_string()
}

// ─── Layer 2: end-to-end quickstart against a real daemon (AC3) ─────────────

#[test]
#[ignore = "spawns loom-daemon + loom CLI subprocesses; see file header for build commands"]
fn quickstart_block_executes_end_to_end() {
    let fake_chromium = fake_chromium_bin();

    // Wire chromium to fake-chromium BEFORE start() (env applies to the daemon
    // spawn); the harness owns the hermetic HOME + unique socket.
    let mut harness = DaemonTestHarness::new().env("LOOM_CHROMIUM_PATH", &fake_chromium);
    provision_web_world(harness.home());
    harness.start();

    // 1. `loom session create --profile standard`
    let sid = {
        let out = run_loom(&harness, &["session", "create", "--profile", "standard"]);
        assert_eq!(
            out.status,
            0,
            "session create must exit 0; stderr={:?} daemon_stderr=\n{}",
            out.stderr,
            harness_daemon_stderr(&harness)
        );
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| {
            panic!("session create stdout not JSON: {e}; raw={:?}", out.stdout)
        });
        v.get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or_else(|| panic!("session_id missing in create response: {v}"))
            .to_string()
    };

    // 2. `loom action web.navigate --session $SESSION --url <URL>` — the AC3
    //    command. Assert a successful receipt (hermetic 200 via fake-chromium).
    let receipt = {
        let out = run_loom(
            &harness,
            &[
                "action",
                "web.navigate",
                "--session",
                &sid,
                "--url",
                "http://fake.test/status/200",
            ],
        );
        assert_eq!(
            out.status,
            0,
            "web.navigate (single --session form) must exit 0 with a success \
             receipt; stderr={:?} daemon_stderr=\n{}",
            out.stderr,
            harness_daemon_stderr(&harness)
        );
        serde_json::from_str::<serde_json::Value>(&out.stdout)
            .unwrap_or_else(|e| panic!("navigate stdout not JSON: {e}; raw={:?}", out.stdout))
    };
    assert_eq!(
        receipt["status"], "success",
        "navigate receipt status must be 'success'; got: {receipt}"
    );
    assert_eq!(
        receipt["status_code"], 200u64,
        "navigate receipt status_code must be 200; got: {receipt}"
    );
    assert!(
        receipt["error"].is_null(),
        "navigate receipt error must be null on success; got: {receipt}"
    );

    // 3. `loom action web.evaluate --session $SESSION --expression <EXPR>` — the
    //    quickstart's second action. The README evaluates `document.title`;
    //    fake-chromium does not execute JS, so we drive its document-title
    //    stand-in sentinel `__loom_test_doc_title__` (→ "My Page"), the
    //    evaluate analogue of the `fake.test/status/200` navigate stand-in. This
    //    proves the single-`--session` form parses and round-trips a successful
    //    evaluate receipt; the literal `document.title` form is locked by the
    //    no-spawn README guard above.
    {
        let out = run_loom(
            &harness,
            &[
                "action",
                "web.evaluate",
                "--session",
                &sid,
                "--expression",
                "__loom_test_doc_title__",
            ],
        );
        assert_eq!(
            out.status,
            0,
            "web.evaluate (single --session form) must exit 0 with a success \
             receipt; stderr={:?} daemon_stderr=\n{}",
            out.stderr,
            harness_daemon_stderr(&harness)
        );
        let v: serde_json::Value = serde_json::from_str(&out.stdout)
            .unwrap_or_else(|e| panic!("evaluate stdout not JSON: {e}; raw={:?}", out.stdout));
        assert_eq!(
            v["status"], "success",
            "evaluate receipt status must be 'success'; got: {v}"
        );
        assert_eq!(
            v["return_value_json"], "\"My Page\"",
            "evaluate must round-trip the document-title sentinel value; got: {v}"
        );
    }

    // 4. `loom session close $SESSION`
    {
        let out = run_loom(&harness, &["session", "close", &sid]);
        assert_eq!(
            out.status,
            0,
            "session close must exit 0; stderr={:?} daemon_stderr=\n{}",
            out.stderr,
            harness_daemon_stderr(&harness)
        );
    }
    // `harness` drop stops the daemon and removes the hermetic HOME.
}

// ─── CLI invocation helper ──────────────────────────────────────────────────

struct CliOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

/// Run a `loom` subcommand through the harness (hermetic env + `LOOM_SOCKET_PATH`
/// wired to the daemon), forcing canonical JSON output so receipts parse
/// deterministically regardless of TTY detection.
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

fn harness_daemon_stderr(harness: &DaemonTestHarness) -> String {
    std::fs::read_to_string(harness.home().join("daemon.stderr")).unwrap_or_default()
}

// ─── Hermetic web-world provisioning ────────────────────────────────────────
//
// The DaemonTestHarness gives us a hermetic HOME + a started daemon, but it
// does not lay down the surfaces / schemas the web.* verbs need. We provision
// them into the harness's HOME before `start()`, at the per-OS-stable paths the
// daemon hardcodes (`$HOME/.config/loom/{surfaces,schemas/v1}` on BOTH macOS and
// Linux — see loom-daemon/src/lib.rs, which deliberately avoids
// `dirs::config_dir()` because it differs on macOS). Chromium is wired via the
// `LOOM_CHROMIUM_PATH` env override (set by the caller) so no per-OS chromium
// symlink dance is needed. This mirrors `integration_naverr_cli_e2e`'s Sandbox;
// a future cleanup could extract a shared `common/web_world.rs`.

fn provision_web_world(home: &Path) {
    let cfg_loom = home.join(".config").join("loom");

    // Surfaces: symlink the AOT-compiled web-surface cwasm into the dir the
    // daemon's ModuleLibrary scans.
    let surfaces_dir = cfg_loom.join("surfaces");
    std::fs::create_dir_all(&surfaces_dir).expect("mkdir surfaces");
    std::os::unix::fs::symlink(cwasm_path(), surfaces_dir.join("loom_surface_web.cwasm"))
        .expect("symlink cwasm into surfaces");

    // Schemas: emit BUILTIN_SCHEMAS (web.* + rpc.schemas) so the daemon's
    // SchemaValidator registers the web verbs, plus permissive schemas for the
    // session lifecycle methods the quickstart drives (the daemon gates
    // unschema'd methods as MethodNotFound once the registry is non-empty).
    let schemas_dir = cfg_loom.join("schemas").join("v1");
    std::fs::create_dir_all(&schemas_dir).expect("mkdir schemas/v1");
    loom_cli::postinstall_runner::schema_step(&schemas_dir).expect("schema_step");
    let permissive = r#"{"request":{"type":"object","additionalProperties":true},"response":{"type":"object","additionalProperties":true}}"#;
    for method in [
        "session.create",
        "session.close",
        "session.list",
        "session.validate",
    ] {
        std::fs::write(schemas_dir.join(format!("{method}.json")), permissive)
            .expect("write permissive session schema");
    }
}

// ─── Built-once fixture: binary paths + AOT-compiled cwasm ──────────────────

fn fake_chromium_bin() -> PathBuf {
    let path = target_bin_dir().join("fake-chromium");
    assert!(
        path.exists(),
        "fake-chromium not built at {}; run \
         `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium`",
        path.display()
    );
    path
}

/// AOT-compile the wasm32-wasip2 web surface to a `.cwasm` once per test binary,
/// cached under the target dir; returns its path. Uses loom-host's `Compiler`
/// so the artifact is byte-compatible with the runtime the daemon loads.
fn cwasm_path() -> &'static Path {
    static CWASM: OnceLock<PathBuf> = OnceLock::new();
    CWASM.get_or_init(|| {
        let wasm_path = workspace_root().join("target/wasm32-wasip2/release/loom_surface_web.wasm");
        assert!(
            wasm_path.exists(),
            "wasm32-wasip2 surface artifact not built at {}; run \
             `cargo build --target wasm32-wasip2 -p loom-surface-web --release`",
            wasm_path.display()
        );

        let cwasm =
            workspace_root().join("target/loom-quickstart-e2e-cwasm/loom_surface_web.cwasm");
        std::fs::create_dir_all(cwasm.parent().unwrap()).expect("create cwasm cache dir");
        if !cwasm.exists() {
            use loom_host::compiler::Compiler;
            use loom_host::wasm_runtime::{WasmRuntime, WasmRuntimeConfig};
            let runtime = WasmRuntime::new(WasmRuntimeConfig::default()).expect("WasmRuntime::new");
            let compiler = Compiler::new(runtime);
            compiler
                .compile_module(&wasm_path, &cwasm)
                .expect("AOT compile loom_surface_web.wasm -> cwasm");
        }
        cwasm
    })
}

/// `<target>/debug/deps/<test>-<hash>` → bin dir is two parents up.
fn target_bin_dir() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/<profile> dir")
        .to_path_buf()
}

/// `CARGO_MANIFEST_DIR` is `<workspace>/loom-cli`; workspace root is its parent.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}
