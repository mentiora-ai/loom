//! `loom-daemon` — Loom daemon entry point.
//!
//! Wires `loom-core` + `loom-host` + `loom-rpc` into a running
//! Unix-socket JSON-RPC server. Invoked by `loom serve` .
//!
//! Startup sequence:
//!   1. Parse `--socket` / `--config` args.
//!   2. Construct `CoreApiFacade` (crash-recovery sweep included).
//!   3. Construct `WasmHost` (loads pre-compiled `.cwasm` modules).
//!      On load failure, surfaces return `SurfaceUnavailable` until
//!      `loom postinstall` compiles them.
//!   4. Wire `ConnectionHandlerDeps` (adapters → handlers → router →
//!      auth middleware → schema validator → observability).
//!   5. Bind the Unix socket (`SocketServer::new`).
//!   6. Print `HELLO_TOKEN=<hex>` to stdout .
//!   7. Block on the accept loop until SIGINT / SIGTERM.

use std::sync::Arc;
use std::sync::OnceLock;

/// Maximum number of concurrently-active sessions a single daemon will hold.
/// Caps unbounded chromium/context growth (each session spawns a chromium shim
/// plus a `/tmp/loom-chromium-*` profile dir). Overridable via
/// `LOOM_MAX_CONCURRENT_SESSIONS`; default 16. A cap-hit fails fast with the
/// typed `SessionCapExceeded` (wire `session_cap_exceeded`, retryable via
/// back-off — reconnecting can't free a slot) carrying `{active, cap, hint}`.
pub(crate) fn max_concurrent_sessions() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("LOOM_MAX_CONCURRENT_SESSIONS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(16)
    })
}

pub mod reaper;
mod upload_guard;
mod vault_bridge;

// ─── Submodules (large-file split of lib.rs) ─────────────────────────────────
//
// Pure module reorganization — no behavior change. Each module owns a cohesive
// slice of the former monolith; the `pub(crate) use *::*` glob re-exports keep
// every existing reference in `async_main`, the DI wiring, AND the
// `#[cfg(test)] mod tests` block (which uses `use super::*`) resolving unchanged.
mod auth_perms;
mod cli_args;
mod core_bridge;
mod health;
mod tts_backend;
mod wasm_bridge;
mod wire_receipts;

pub(crate) use auth_perms::*;
pub(crate) use cli_args::*;
pub(crate) use core_bridge::*;
pub(crate) use health::*;
pub(crate) use wasm_bridge::*;
// Re-exported solely so the `#[cfg(test)] mod tests` block (which uses
// `use super::*`) reaches the receipt/payload builders. Non-test daemon code
// imports them directly from `crate::wire_receipts` (see `wasm_bridge`), so
// outside test builds this glob has no consumer here.
#[cfg(test)]
pub(crate) use wire_receipts::*;

use anyhow::{Context, Result};
use loom_core::core_api_facade::{CoreApiFacade, CoreConfig};
use loom_core::error::LoomError;
use loom_rpc::auth_middleware::auth_middleware::{AuthMiddleware, Token};
use loom_rpc::connection_handler::connection_handler::ConnectionHandlerDeps;
use loom_rpc::core_service_adapter::core_service_adapter::{AdapterError, CoreServiceAdapter};
use loom_rpc::host_service_adapter::host_service_adapter::{HostServiceAdapter, WasmHostBridge};
use loom_rpc::request_router::request_router::RequestRouter;
use loom_rpc::rpc_handlers::rpc_handlers::RpcHandlers;
use loom_rpc::rpc_handlers::rpc_handlers::{DaemonHealthAsync, DaemonHealthProvider};
use loom_rpc::rpc_observability::rpc_observability::RpcObservability;
use loom_rpc::schema_provider::schema_provider::SchemaProvider;
use loom_rpc::schema_validator::schema_validator::SchemaValidator;
use loom_rpc::socket_server::socket_server::{SocketServer, SocketServerConfig};

// ─── Vault threat-model startup precondition ─────────
//
// The file is embedded at compile time so the daemon binary cannot be built
// without `security/vault_threat_model.md`. At runtime we also require the
// four section headings — together this ensures the runtime
// `threat_model_acknowledged: true` stamp on `vault.grant` is provably
// grounded in a present, well-formed threat-model document.

const VAULT_THREAT_MODEL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../security/vault_threat_model.md"
));

fn check_vault_threat_model() -> Result<()> {
    const REQUIRED_SECTIONS: &[&str] = &[
        "## Attacker Classes",
        "## Security Goals",
        "## Trust Boundaries",
        "## Abuse Cases",
    ];
    if !VAULT_THREAT_MODEL.starts_with("# Vault Threat Model") {
        anyhow::bail!("vault_threat_model.md must start with '# Vault Threat Model'");
    }
    for section in REQUIRED_SECTIONS {
        if !VAULT_THREAT_MODEL.contains(section) {
            anyhow::bail!(
                "vault_threat_model.md missing required section heading: {}",
                section
            );
        }
    }
    Ok(())
}

pub(crate) fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Map a `loom-core::LoomError` → `loom-rpc::LoomErrorCode`.
// pub(crate): shared with the vault_bridge submodule (large-file split).
pub(crate) fn map_loom_error(e: &LoomError) -> AdapterError {
    use loom_core::error::LoomErrorCode as CoreCode;
    use loom_rpc::error_translator::error_translator::LoomErrorCode as RpcCode;
    match e.code {
        CoreCode::SessionNotFound | CoreCode::SessionKilled => RpcCode::SessionNotFound,
        CoreCode::SessionAlreadyClosed => RpcCode::SessionClosed,
        CoreCode::SessionAborted => RpcCode::SessionAborted,
        CoreCode::BudgetExceeded | CoreCode::BudgetRateLimited => RpcCode::BudgetExceeded,
        CoreCode::StoreIntegrityFailed | CoreCode::ManifestCorrupt => RpcCode::StoreIntegrityFailed,
        // Distinct kinds: revoke and expire must be
        // distinguishable on the wire. F-A2 / F-S1 / F-S2 fix —
        // previously these collapsed into VaultGrantNotFound.
        //
        // VaultUnknownLabel: keychain has no credential under the
        // requested label. Today (NullKeychain in the daemon's vault
        // wiring) this fires for EVERY vault.grant call until the
        // OAuth device flow lands and populates the keychain via
        // `vault.add`. The wire kind is `vault_grant_not_found` for
        // backward compat, but the structured detail (when surfaced
        // by error_mapper) calls out the missing-credential reason
        // so operators don't chase a phantom grant id.
        CoreCode::VaultUnknownLabel => RpcCode::VaultGrantNotFound,
        CoreCode::VaultGrantRevoked => RpcCode::VaultGrantRevoked,
        CoreCode::VaultGrantExpired => RpcCode::VaultGrantExpired,
        CoreCode::VaultRejection => RpcCode::VaultRejection,
        // Surface trap (genuine wasmtime trap OR guest-returned
        // host-error::shim-failure / store-integrity-failed / etc.
        // that decode_typed_receipt mapped). The rpc-layer
        // LoomErrorCode lacks a dedicated ShimFailure / ShimTimeout
        // variant today, so all shim-derived faults surface as
        // SurfaceTrap; expand this mapping when the rpc enum grows.
        CoreCode::SurfaceTrap
        | CoreCode::ShimFailure
        | CoreCode::ShimTimeout
        | CoreCode::ShimBreakerOpen => RpcCode::SurfaceTrap,
        // Per-action deadline kill: the executor traps with `RequestTimeout`
        // when an action exceeds its `deadline_ms`. Identity arm so the typed
        // `request_timeout` survives daemon → wire translation instead of
        // collapsing to the `_ => InternalError` catch-all (which would mask a
        // deliberate deadline kill as an internal fault). Distinct from the
        // RPC-connection-envelope `request_timeout` in `connection_handler`,
        // which abandons the RPC future rather than killing the action.
        CoreCode::RequestTimeout => RpcCode::RequestTimeout,
        // profile-restricted is a wire-stable kind
        // that survives daemon → wire translation. Detail (matched_pattern,
        // profile, violation) is currently constructed at the daemon gate
        // site and lives in `Receipt.error.detail`, not in the LoomError
        // context — this arm only matters if a downstream emitter routes
        // ProfileRestricted through `LoomError`.
        CoreCode::ProfileRestricted => RpcCode::ProfileRestricted,
        // Wire-stable replay-refusal kind (the replay path itself bypasses
        // this map — see `replay_session_to_id` — but keep the arm 1:1 so a
        // future emit site can never degrade it to the InternalError
        // catchall).
        CoreCode::NotReplayable => RpcCode::NotReplayable,
        CoreCode::Unsupported => RpcCode::SurfaceUnavailable,
        // InvalidArgument carries a typed message (e.g. "unsupported
        // export format: cdp"). Map to SchemaViolation on the wire so
        // the receipt's `code` field reflects what's wrong with the
        // request rather than collapsing to the generic `internal_error`.
        // (`InvalidArgument` previously fell into the catchall arm,
        // surfacing as "Error: internal_error: session.export failed
        // for session ..." which gives the operator no actionable
        // signal about what to change.)
        CoreCode::InvalidArgument => RpcCode::SchemaViolation,
        // Already wire-shaped: the cap rejection is emitted with its final
        // code (defensive identity arm — without it the catch-all would
        // collapse a re-routed cap error back to the opaque internal_error).
        CoreCode::SessionCapExceeded => RpcCode::SessionCapExceeded,
        _ => RpcCode::InternalError,
    }
}

/// Like [`map_loom_error`], but keeps the full error (message + context)
/// alongside the translated wire code — for bridge methods whose signature
/// carries `LoomError` (today: `create_session_raw`) so structured detail
/// survives to the JSON-RPC envelope instead of collapsing to a bare code.
pub(crate) fn map_loom_error_full(e: &LoomError) -> LoomError {
    LoomError {
        code: map_loom_error(e),
        message: e.message.clone(),
        context: e.context.clone(),
    }
}

// ─── Public entry point ──────────────────────────────────────────────────────
//
// exposed as `pub fn run()` so the `loom-daemon` binary can live
// in `loom-cli/src/bin/loom-daemon.rs` (a thin shim) and cargo-dist 0.30+
// ships all 4 loom binaries from one Cargo Package in one tarball — its docs
// require all `[[bin]]` entries to be in one Package to bundle.

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();

    // Short-circuit on --help / --version BEFORE the vault check + socket
    // bind. Otherwise a user typing `loom-daemon --help` either spawns a
    // long-lived daemon (no daemon already running) or fails opaquely with
    // `AddressInUse` (one is). Neither is what --help should do.
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print_daemon_help();
        return Ok(());
    }
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("loom-daemon {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    //.1 startup gate (F-S6): refuse to start without a
    // present, well-formed threat-model document.
    check_vault_threat_model().context("vault threat-model precondition failed")?;

    let args = parse_args(&argv);

    // Init tracing to stderr so stdout stays clean for HELLO_TOKEN.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Ensure data directories exist.
    std::fs::create_dir_all(&args.data_root)
        .with_context(|| format!("create data_root {}", args.data_root.display()))?;
    if let Some(parent) = args.log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = args.socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket dir {}", parent.display()))?;
    }

    // 1a. Resolve the keychain backend per LOOM_KEYCHAIN_BACKEND +
    //     LOOM_KEYCHAIN_ALLOW_PROMPT. When an OS-backed backend is
    //     explicitly requested (`macos` | `linux` | `auto`), init failure
    //     is hard-fail-closed — no silent fallback to a stub (per D7).
    //     When the env var is UNSET, default to `in_memory` so the
    //     daemon starts in CI / dev-test contexts that don't have a
    //     keychain daemon running. Production deployments must opt in
    //     explicitly via `LOOM_KEYCHAIN_BACKEND=auto` (or =macos / =linux).
    let keychain_cfg = {
        use std::io::IsTerminal;
        let backend = match std::env::var("LOOM_KEYCHAIN_BACKEND").ok().as_deref() {
            Some("stub") => loom_keychain::BackendChoice::Stub,
            Some("in_memory") => loom_keychain::BackendChoice::InMemory,
            Some("macos") => loom_keychain::BackendChoice::MacOs,
            Some("linux") => loom_keychain::BackendChoice::Linux,
            Some("auto") => loom_keychain::KeychainConfig::default().backend,
            Some(other) => {
                anyhow::bail!(
                    "loom-daemon: unknown LOOM_KEYCHAIN_BACKEND={other}; \
                     expected one of: stub | in_memory | macos | linux | auto"
                );
            }
            None => loom_keychain::BackendChoice::InMemory,
        };
        let allow_prompt = match std::env::var("LOOM_KEYCHAIN_ALLOW_PROMPT").ok().as_deref() {
            Some("0") | Some("false") => false,
            Some("1") | Some("true") => true,
            Some(other) => {
                anyhow::bail!(
                    "loom-daemon: invalid LOOM_KEYCHAIN_ALLOW_PROMPT={other}; expected 0|1"
                );
            }
            None => std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        };
        loom_keychain::KeychainConfig {
            backend,
            allow_prompt,
            service_id: "loom",
        }
    };
    let keychain = match loom_keychain::select_keychain(&keychain_cfg) {
        Ok(kc) => {
            tracing::info!(
                backend = ?keychain_cfg.backend,
                service_id = keychain_cfg.service_id,
                allow_prompt = keychain_cfg.allow_prompt,
                "loom-daemon: keychain backend initialised"
            );
            kc
        }
        Err(e) => {
            tracing::error!(
                backend = ?keychain_cfg.backend,
                error = %e,
                "loom-daemon: keychain backend failed to initialise; refusing to start"
            );
            anyhow::bail!(
                "loom-daemon: {:?} keychain backend failed to initialise: {}. \
                 Set LOOM_KEYCHAIN_BACKEND=stub to run without keychain persistence \
                 (NOT recommended for production).",
                keychain_cfg.backend,
                e
            );
        }
    };

    // 1b. Build CoreApiFacade with the resolved keychain.
    let core_config = CoreConfig {
        data_root: args.data_root.clone(),
        log_path: args.log_path.clone(),
        otel_enabled: args.otel_enabled,
        default_seed: args.default_seed,
        checkpoint_every_n: args.checkpoint_every_n,
    };
    let core = CoreApiFacade::new(core_config, keychain).context("CoreApiFacade::new failed")?;

    // 2. Crash-recovery sweep. Recovery errors are non-fatal — the daemon
    //    continues serving — but the report is logged (not discarded) so an
    //    operator can see crashed/quarantined counts at startup.
    match core.startup_manager.perform_recovery_sweep() {
        Ok(report) => {
            if report.sessions_crashed > 0
                || report.sessions_quarantined > 0
                || !report.failed_sessions.is_empty()
                || report.orphan_tmpfiles_removed > 0
            {
                tracing::warn!(
                    metric = "loom_daemon_recovery_sweep",
                    sessions_recovered = report.sessions_recovered,
                    sessions_crashed = report.sessions_crashed,
                    sessions_quarantined = report.sessions_quarantined,
                    orphan_tmpfiles_removed = report.orphan_tmpfiles_removed,
                    failed_sessions = report.failed_sessions.len(),
                    "startup crash-recovery sweep completed"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "startup crash-recovery sweep failed (non-fatal)");
        }
    }

    // 3. Build WasmHost (or stub if surfaces aren't compiled yet).
    let (host_bridge, wasm_host_handle): (Arc<dyn WasmHostBridge>, _) =
        build_host_bridge(Arc::clone(&core), args.upload_root.clone());

    // 3b. Startup orphan-Chromium GC. A previous daemon's unclean exit can leave
    //     `loom-chromium-*` user-data-dirs whose sessions are gone but whose browser trees
    //     still hold fds/pids. The live set is empty here (no sessions created yet), so every
    //     aged loom-chromium dir is an orphan — reap it before serving so a churned host
    //     starts clean. Best-effort; never fatal.
    {
        let reaper_cfg = reaper::ReaperConfig::from_env();
        if reaper_cfg.orphan_gc_enabled {
            let report =
                reaper::run_sweep(&core, wasm_host_handle.as_ref(), &reaper_cfg, true).await;
            if !report.is_empty() {
                tracing::warn!(
                    metric = "loom_reaper_startup_sweep",
                    orphan_browsers_killed = report.orphan_browsers_killed.len(),
                    orphan_dirs_removed = report.orphan_dirs_removed,
                    "startup orphan-Chromium GC reaped leaked browser trees"
                );
            }
        }
    }

    // 4. Build schema provider — EMBEDDED-FIRST (mcp-navigate-schema-regression).
    //    Builtin action methods validate against the schemas compiled into
    //    THIS binary (`loom_shared::builtin_schemas`), so the validator can
    //    never enforce a stale on-disk schema from an earlier install (the
    //    v0.11.0 regression: a pre-settle-capture web.navigate.json rejected
    //    the documented `until`/`timeout_ms` args forever, while the fresher
    //    web.wait_for.json accepted them). Disk files act only as an OVERLAY
    //    for methods unknown to the binary; a builtin-method file whose
    //    content differs is reported below and ignored.
    //
    //    Overlay dir search keeps the historical order: data_root first
    //    (~/Library/Application Support/loom on macOS), then the
    //    postinstall-installed location (~/.config/loom).
    let primary_schema_dir = args.data_root.join("schemas").join("v1");
    // The `loom postinstall` runner installs to ~/.config/loom on every
    // platform (cross-platform parity with the Linux build). On macOS,
    // `dirs::config_dir()` returns `~/Library/Application Support`, NOT
    // `~/.config` — so we hardcode the `$HOME/.config/loom` fallback to
    // match what the postinstall step actually writes.
    let postinstall_schema_dir = std::env::var_os("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".config")
                .join("loom")
                .join("schemas")
                .join("v1")
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".loom-schemas"));
    let overlay_dir = if primary_schema_dir.is_dir() {
        Some(primary_schema_dir.as_path())
    } else if postinstall_schema_dir.is_dir() {
        Some(postinstall_schema_dir.as_path())
    } else {
        None
    };
    let schemas: Arc<dyn loom_rpc::schema_provider::schema_provider::SchemaProviderApi> =
        match SchemaProvider::load_embedded_with_overlay(overlay_dir) {
            Ok((provider, stale_mirrors)) => {
                for stale in &stale_mirrors {
                    tracing::warn!(
                        method = %stale.method,
                        path = %stale.path.display(),
                        "stale schema mirror ignored — this file no longer matches the \
                         schema embedded in this binary, which is what the daemon \
                         validates against. Run `loom postinstall` to refresh the \
                         mirror (or delete the file)."
                    );
                }
                provider
            }
            Err(e) => {
                // An unreadable/uncompilable OVERLAY file must not brick
                // startup OR silently disable validation (the pre-fix
                // EmptySchemas fallback bypassed validation entirely).
                // Degrade to the pure embedded baseline — strictly stronger
                // than both old behaviors.
                tracing::error!(
                    error = ?e,
                    "schema overlay load failed — continuing with embedded builtin \
                     schemas only (overlay extras unavailable)"
                );
                SchemaProvider::load_embedded()
                    .map_err(|e| anyhow::anyhow!("embedded schema load failed: {:?}", e))?
            }
        };

    // 5. Wire DI graph.
    let core_adapter = CoreServiceAdapter::new(Arc::new(CoreBridge {
        core: Arc::clone(&core),
        wasm_host: wasm_host_handle.clone(),
        cleanup_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
    }));
    let host_adapter = HostServiceAdapter::new(host_bridge);
    let validator: Arc<dyn loom_rpc::schema_validator::schema_validator::SchemaValidatorApi> =
        SchemaValidator::new(Arc::clone(&schemas));
    let obs: Arc<dyn loom_rpc::rpc_observability::rpc_observability::RpcObservabilityApi> =
        RpcObservability::new();
    let handlers = RpcHandlers::new(
        core_adapter,
        host_adapter,
        Arc::clone(&schemas),
        Arc::clone(&validator),
        Arc::clone(&obs),
    );
    // Wire the async shim-teardown driver so `session.kill` can await
    // shim child reap with a 5 s ceiling per D12. When wasm_host is None
    // (chromium not yet postinstalled), session.kill degrades to
    // abort-only — caller still gets a typed envelope back.
    if let Some(host) = wasm_host_handle.clone() {
        let _ = handlers.set_session_shutdown(Arc::new(WasmHostShutdownAdapter { host }));
    }
    // Wire the daemon.health snapshot provider. Always wireable —
    // wasm_host being None just means `shim_breaker_states` returns
    // empty. Active-session count comes from the core facade regardless.
    // One bridge instance, two trait wirings (sync shallow + async deep).
    let bridge = Arc::new(DaemonHealthBridge {
        core: Arc::clone(&core),
        wasm_host: wasm_host_handle.clone(),
    });
    let _ = handlers.set_health_provider(bridge.clone() as Arc<dyn DaemonHealthProvider>);
    let _ = handlers.set_daemon_health_async(bridge as Arc<dyn DaemonHealthAsync>);
    let router: Arc<dyn loom_rpc::request_router::request_router::RequestRouterApi> =
        RequestRouter::register_methods(
            Arc::clone(&handlers),
            Arc::clone(&schemas),
            Arc::clone(&validator),
        )
        .map_err(|e| anyhow::anyhow!("RequestRouter::register_methods failed: {:?}", e))?;

    // 6. Bind socket. Generate token once; share between auth + socket config.
    let token = Token::generate();
    let token_arc = Arc::new(token.clone());
    let auth: Arc<dyn loom_rpc::auth_middleware::auth_middleware::AuthMiddlewareApi> =
        AuthMiddleware::new(Arc::clone(&token_arc));
    let socket_config = SocketServerConfig {
        socket_path: args.socket_path.clone(),
        token_override: Some(token),
    };
    let deps = Arc::new(ConnectionHandlerDeps {
        auth,
        validator: Arc::clone(&validator),
        router,
        observability: Arc::clone(&obs),
    });
    let server = SocketServer::new(socket_config, deps)
        .map_err(|e| anyhow::anyhow!("SocketServer::new failed: {:?}", e))?;

    // 7. Write auth artefacts for CLI (per the AuthManager contract):
    //    hello.token + daemon.pid in data_root/auth/.
    let auth_dir = args.data_root.join("auth");
    std::fs::create_dir_all(&auth_dir)
        .with_context(|| format!("create auth dir {}", auth_dir.display()))?;
    let token_path = auth_dir.join("hello.token");
    let pid_path = auth_dir.join("daemon.pid");

    // 7a. A-W8.1 / W8.5 0600 startup probe: refuse to start if a pre-
    //     existing auth file has loose mode bits (group/world readable
    //     or writable). Catches the "operator rsync'd $HOME with default
    //     umask and lost the 0600" class of incidents BEFORE the token
    //     is reused. Crash-only; no auto-chmod (the operator must
    //     consciously remediate so the audit trail records intent).
    probe_auth_perms_or_refuse(&token_path, "hello.token")?;
    probe_auth_perms_or_refuse(&pid_path, "daemon.pid")?;

    // 7b. A-W8.1 second leg: CREATE the files with 0600 atomically
    //     (OpenOptions mode on unix). The umask on default Linux installs
    //     is 0022 → a plain fs::write landed at 0644 and a follow-up chmod
    //     left a transient window in which group + world could read (and
    //     keep an fd on) the daemon's sole bearer credential. Creating
    //     with the right mode matches the socket's 0600 contract
    //     (SOCKET_MODE in loom-rpc) with no repair window.
    write_auth_file_0600(&token_path, server.token.0.as_bytes(), "hello.token")?;
    write_auth_file_0600(
        &pid_path,
        std::process::id().to_string().as_bytes(),
        "daemon.pid",
    )?;

    // 8. Print HELLO_TOKEN to stdout .
    println!("HELLO_TOKEN={}", server.token.0);

    // 9. Signal handler for graceful shutdown. SIGTERM is what launchd
    //    stop, systemd stop, and a plain `kill` against daemon.pid all
    //    deliver; SIGINT covers interactive Ctrl-C. Both resolve the same
    //    future so every routine service stop takes the graceful path
    //    below (drain accept loop, abort reaper, remove auth artefacts)
    //    instead of a hard kill that tears in-flight WAL appends.
    let shutdown = shutdown_signal();

    // 9b. Periodic reaper sweep: idle-session eviction + zombie detection + orphan-Chromium
    //     GC on a fixed cadence so a long-running daemon under churn stays healthy without
    //     manual intervention. Runs as a background task aborted on shutdown (below). Skipped
    //     entirely when neither idle-TTL nor orphan-GC is enabled.
    let reaper_task = {
        let reaper_cfg = reaper::ReaperConfig::from_env();
        if reaper_cfg.periodic_enabled() {
            let core_for_reaper = Arc::clone(&core);
            let host_for_reaper = wasm_host_handle.clone();
            tracing::info!(
                idle_ttl_secs = reaper_cfg.idle_ttl.as_secs(),
                sweep_secs = reaper_cfg.sweep_interval.as_secs(),
                orphan_gc = reaper_cfg.orphan_gc_enabled,
                "reaper: periodic sweep enabled"
            );
            Some(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(reaper_cfg.sweep_interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // First tick fires immediately; skip it so we don't double-run the
                // startup sweep that already executed above.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let report = reaper::run_sweep(
                        &core_for_reaper,
                        host_for_reaper.as_ref(),
                        &reaper_cfg,
                        true,
                    )
                    .await;
                    if !report.is_empty() {
                        tracing::info!(
                            metric = "loom_reaper_periodic_sweep",
                            idle_evicted = report.idle_evicted.len(),
                            zombies_closed = report.zombies_closed.len(),
                            orphan_browsers_killed = report.orphan_browsers_killed.len(),
                            orphan_dirs_removed = report.orphan_dirs_removed,
                            "reaper: periodic sweep reaped leaked resources"
                        );
                    }
                }
            }))
        } else {
            None
        }
    };

    // 10. Serve.
    let handle = tokio::runtime::Handle::current();
    server.serve(handle, shutdown).await;

    // 10b. Stop the reaper task on shutdown so it doesn't outlive the runtime.
    if let Some(task) = reaper_task {
        task.abort();
    }

    // 11. Cleanup auth artefacts on shutdown.
    let _ = std::fs::remove_file(&token_path);
    let _ = std::fs::remove_file(&pid_path);

    Ok(())
}

/// Future that resolves when a shutdown signal arrives: SIGINT (Ctrl-C)
/// or — on unix — SIGTERM (the launchd/systemd/`kill` default). Fulfils
/// the module-doc contract "Block on the accept loop until SIGINT /
/// SIGTERM". Handler-installation failure logs and falls back to the
/// other signal instead of panicking: an `.expect()` here would panic
/// the shutdown future inside `SocketServer::serve`'s `select!` and
/// tear down the accept loop.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install Ctrl-C handler; relying on SIGTERM");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        let sigterm = async {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to install SIGTERM handler; relying on Ctrl-C");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            () = ctrl_c => {}
            () = sigterm => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod upload_guard_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use loom_shared::shim_protocol::CdpMessage;
    // Names the tests use that, post-split, no longer flow through the
    // non-test `lib.rs` `use` block (their former non-test users — the
    // `CoreBridge` / `WasmBridge` impls — moved to sibling submodules).
    // `CoreFacadeBridge` is imported for trait-method resolution on the
    // `CoreBridge` close/abort/create tests.
    use loom_rpc::core_service_adapter::core_service_adapter::{
        CoreFacadeBridge, CreateSessionParams,
    };
    use loom_rpc::host_service_adapter::host_service_adapter::{Action, Receipt};
    use std::path::PathBuf;

    // ─── Per-action deadline kill → typed request_timeout on the wire ──────────
    // The executor traps an over-deadline action with LoomErrorCode::RequestTimeout
    // (see loom-host session_executor). map_loom_error must carry that to the wire
    // code `request_timeout` rather than collapsing to the InternalError catch-all
    // (which would mask a deliberate deadline kill as an internal fault).
    #[test]
    fn map_loom_error_preserves_request_timeout() {
        use loom_core::error::{LoomError, LoomErrorCode};
        let e = LoomError::new(
            LoomErrorCode::RequestTimeout,
            "action deadline_ms of 2000 ms exceeded before the action completed".to_string(),
        );
        let wire = map_loom_error(&e);
        assert_eq!(wire, LoomErrorCode::RequestTimeout);
        assert_eq!(wire.as_wire(), "request_timeout");
    }

    /// Serializes tests that mutate process-global env (`std::env::set_var`/`remove_var`).
    /// Env is per-process, so under parallel test execution (cargo's default, or
    /// `cargo test` without `--test-threads=1`) two such tests clobber each other. Every
    /// env-mutating test in this module acquires this lock for the duration of its
    /// mutate→read→restore window. (nextest runs each test in its own process and is
    /// immune regardless; this keeps plain `cargo test` solid too.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ─── Vault label validation (D37) ──────────
    // Direct coverage for the canonical rule shared by vault_set_secret /
    // vault_delete_secret via validate_label_or_rpc_err.

    #[test]
    fn validate_label_canonical_accepts_valid_labels() {
        for ok in ["gh", "github:token", "my-label_1", &"a".repeat(64)] {
            assert!(
                validate_label_canonical(ok).is_ok(),
                "expected {ok:?} to be accepted"
            );
        }
    }

    #[test]
    fn validate_label_canonical_rejects_invalid_labels() {
        assert!(validate_label_canonical("").is_err(), "empty");
        assert!(
            validate_label_canonical(&"a".repeat(65)).is_err(),
            "over 64 chars"
        );
        for bad in [
            "has space",
            "slash/here",
            "dot.here",
            "emoji😀",
            "tab\there",
        ] {
            assert!(
                validate_label_canonical(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_label_or_rpc_err_maps_rejection_to_invalid_argument() {
        // Valid label passes through.
        assert!(validate_label_or_rpc_err("gh").is_ok());
        // Invalid label maps to the same adapter error as InvalidArgument.
        let err = validate_label_or_rpc_err("bad/label").expect_err("should reject");
        let expected = map_loom_error(&LoomError::new(
            loom_core::error::LoomErrorCode::InvalidArgument,
            "x",
        ));
        assert_eq!(
            err, expected,
            "rejection must map to the same adapter error code as InvalidArgument"
        );
    }

    // ─── Daemon-layer evaluate gate (Layer B) ──────────

    /// receipt envelope shape after daemon-side rejection.
    /// Pins the wire fields the operator's `loom action web.evaluate`
    /// reproducer reads from stdout JSON.
    #[test]
    fn profile_restricted_evaluate_receipt_carries_required_fields() {
        let receipt = profile_restricted_evaluate_receipt(42, "01HZSESSION", "window.location");
        assert_eq!(receipt.action_id, 42);
        assert_eq!(receipt.session_id, "01HZSESSION");
        assert!(matches!(
            receipt.status,
            loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus::Error
        ));
        let err = receipt.error.expect("error envelope present");
        assert_eq!(err.kind, "profile_restricted");
        let detail = err.detail.expect("detail present");
        assert_eq!(detail["matched_pattern"], "window.location");
        assert_eq!(detail["profile"], "safe");
        assert_eq!(detail["violation"], "safe_profile_evaluate_denylist_match");
        // Tier-2 navigate fields should all be None on a synthesized
        // error receipt — no DOM/screenshot/network on a refused action.
        assert!(receipt.url.is_none());
        assert!(receipt.dom_snapshot_hash.is_none());
        assert!(receipt.network_summary.is_none());
        assert_eq!(receipt.timing_ticks, 0);
    }

    /// verify the operator's exact reproducer pattern
    /// matches the daemon's denylist BEFORE shim dispatch (through
    /// `find_denylist_match`, the exact routine the gate calls).
    #[test]
    fn evaluate_denylist_blocks_operator_reproducer_window_location_assignment() {
        let expr = "window.location.href = \"https://evil.example.com\"";
        let matched = loom_shared::safety::find_denylist_match(expr);
        assert_eq!(matched, Some("window.location"));
    }

    /// whitespace/comment-smuggled variants hit the same gate — the
    /// normalized second pass of `find_denylist_match` (audit
    /// 2026-06-10: the gate was raw-substring only).
    #[test]
    fn evaluate_denylist_blocks_token_separator_smuggling() {
        for expr in [
            "window . location = 'https://evil.example.com'",
            "document/**/.cookie = ''",
            "eval ('alert(1)')",
        ] {
            assert!(
                loom_shared::safety::find_denylist_match(expr).is_some(),
                "smuggled variant must be blocked: {expr:?}"
            );
        }
    }

    /// service-worker registration is gated; feature detection is allowed.
    #[test]
    fn evaluate_denylist_gates_service_worker_register_not_feature_detect() {
        let register = "navigator.serviceWorker.register('/sw.js')";
        let detect = "if ('serviceWorker' in navigator) {}";
        assert!(
            loom_shared::safety::find_denylist_match(register).is_some(),
            "registration must be blocked"
        );
        assert!(
            loom_shared::safety::find_denylist_match(detect).is_none(),
            "feature detection must NOT be blocked"
        );
    }

    /// Wire string — `LoomErrorCode::ProfileRestricted`
    /// serializes as `"profile_restricted"`. Mirrors what the receipt's
    /// `error.kind` carries; if these drift, the operator's grep on
    /// the receipt JSON breaks.
    #[test]
    fn loom_error_code_profile_restricted_wire_string_matches_receipt_kind() {
        use loom_shared::error_format::LoomErrorCode;
        assert_eq!(
            LoomErrorCode::ProfileRestricted.as_wire(),
            "profile_restricted"
        );
    }

    // ─── v0.9.7 follow-ups A + B — cookie validation + grant ────────────

    /// Each `CookieValidationError` variant maps to a stable snake_case
    /// wire string. The operator's `loom action web.set_cookies` failure
    /// receipt carries `detail.code = <wire string>` so dashboards can
    /// group by validation reason.
    #[test]
    fn cookie_validation_code_covers_all_variants() {
        use loom_shared::cookie_types::CookieValidationError as E;
        assert_eq!(cookie_validation_code(&E::NameEmpty), "name_empty");
        assert_eq!(
            cookie_validation_code(&E::NameInvalid { ch: ';' }),
            "name_invalid"
        );
        assert_eq!(
            cookie_validation_code(&E::ValueTooLarge { size: 5_000 }),
            "value_too_large"
        );
        assert_eq!(
            cookie_validation_code(&E::InvalidSameSite("foo".to_string())),
            "invalid_same_site"
        );
        assert_eq!(
            cookie_validation_code(&E::InvalidExpires(f64::NAN)),
            "invalid_expires"
        );
        assert_eq!(
            cookie_validation_code(&E::TooManyCookies(65)),
            "too_many_cookies"
        );
    }

    /// Receipt envelope shape returned when daemon-side validation
    /// rejects a `web.set_cookies` payload. Pins the wire fields the
    /// CLI reproducer reads (`error.kind` and `error.detail.code`).
    #[test]
    fn cookie_validation_error_receipt_carries_required_fields() {
        let r = cookie_validation_error_receipt(
            7,
            "01HZSESSION",
            "too_many_cookies",
            "65 cookies provided, max is 64".to_string(),
        );
        assert_eq!(r.action_id, 7);
        assert_eq!(r.session_id, "01HZSESSION");
        assert!(matches!(
            r.status,
            loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus::Error
        ));
        let err = r.error.expect("error envelope present");
        assert_eq!(err.kind, "cookie_validation_error");
        let detail = err.detail.expect("detail present");
        assert_eq!(detail["code"], "too_many_cookies");
        assert_eq!(detail["message"], "65 cookies provided, max is 64");
        // Synthesised error — none of the success-path fields populated.
        assert!(r.set_cookies_result.is_none());
        assert!(r.url.is_none());
        assert!(r.dom_snapshot_hash.is_none());
        assert_eq!(r.timing_ticks, 0);
    }

    /// `build_chromium_args` defensively emits an empty no-op envelope
    /// for a `set_cookies` action whose source is still `grant` by the
    /// time it reaches the CDP encoder — the dispatcher should have
    /// resolved it upstream, but tests / future callers may bypass
    /// that path.
    #[test]
    fn build_chromium_args_set_cookies_grant_source_emits_empty_no_op() {
        let action = Action::WebSetCookies {
            session_id: s("sess"),
            source: serde_json::json!({
                "source": "grant",
                "grant_id": "grn_abc",
            }),
        };
        let msg = decode_cdp(&action).expect("envelope produced");
        assert_eq!(msg.method, "Network.setCookies");
        // params.cookies = [] (empty array)
        let cookies = params_get(&msg, "cookies").expect("cookies param present");
        match cookies {
            ciborium::value::Value::Array(arr) => assert!(arr.is_empty()),
            other => panic!("expected empty array, got {other:?}"),
        }
    }

    /// `build_chromium_args` for a resolved inline `set_cookies` passes
    /// the cookies array through to the CDP envelope. This is the
    /// shape the dispatcher hands `build_chromium_args` after grant
    /// resolution.
    #[test]
    fn build_chromium_args_set_cookies_inline_passes_through_cookies() {
        let action = Action::WebSetCookies {
            session_id: s("sess"),
            source: serde_json::json!({
                "source": "inline",
                "cookies": [
                    {"name": "sid", "value": "abc", "domain": "example.com", "path": "/"}
                ],
            }),
        };
        let msg = decode_cdp(&action).expect("envelope produced");
        assert_eq!(msg.method, "Network.setCookies");
        let cookies = params_get(&msg, "cookies").expect("cookies param present");
        match cookies {
            ciborium::value::Value::Array(arr) => {
                assert_eq!(arr.len(), 1);
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    // ─── existing tests ─────────────────────────────────────────────────

    /// Decode `build_chromium_args` output into the wire-shape struct.
    /// Returns None if the function returned None (legacy fallback path).
    fn decode_cdp(action: &Action) -> Option<CdpMessage> {
        let bytes = build_chromium_args(action)?;
        Some(
            ciborium::de::from_reader::<CdpMessage, _>(bytes.as_slice()).expect("valid CdpMessage"),
        )
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

    /// every Web.* variant produces a decodable CdpMessage.
    #[test]
    fn build_chromium_args_emits_valid_cdp_message_for_each_web_verb() {
        let session = s("sess-1");
        let cases: Vec<(Action, &str)> = vec![
            (
                Action::WebNavigate {
                    session_id: session.clone(),
                    url: s("https://example.com"),
                    until: None,
                    timeout_ms: None,
                },
                "Page.navigate",
            ),
            // cdp-trusted-input: web.click is now host-side (trusted CDP
            // Input.dispatchMouseEvent) → build_chromium_args returns None, so
            // it is not part of this "each verb yields a CdpMessage" sweep.
            (
                Action::WebEvaluate {
                    session_id: session.clone(),
                    expression: s("1+1"),
                },
                "Runtime.evaluate",
            ),
            (
                // cdp-trusted-input: the DEFAULT (`mode:None`) is now `fill`
                // (host-side CDP Input.insertText) → build_chromium_args returns
                // None; only the legacy `value` mode builds the Runtime.evaluate
                // setter JS, so this sweep uses `mode:"value"` explicitly.
                Action::WebType {
                    session_id: session.clone(),
                    selector: s("input"),
                    text: s("hello"),
                    mode: Some(s("value")),
                    until: None,
                },
                "Runtime.evaluate",
            ),
            (
                Action::WebSelect {
                    session_id: session.clone(),
                    selector: s("select"),
                    value: s("v1"),
                },
                "Runtime.evaluate",
            ),
            (
                Action::WebHover {
                    session_id: session.clone(),
                    selector: s("a"),
                },
                "Runtime.evaluate",
            ),
            (
                Action::WebScroll {
                    session_id: session.clone(),
                    selector: Some(s("body")),
                    delta_x: Some(0),
                    delta_y: Some(100),
                },
                "Runtime.evaluate",
            ),
            // web.wait is host-intercepted (like web.click) → no guest envelope;
            // its None case is asserted by build_chromium_args_wait_is_host_side_returns_none.
            (
                Action::WebScreenshot {
                    session_id: session.clone(),
                    selector: None,
                },
                "Page.captureScreenshot",
            ),
            (
                Action::WebSnapshot {
                    session_id: session.clone(),
                },
                "DOM.getDocument",
            ),
        ];
        for (action, expected_method) in cases {
            let msg = decode_cdp(&action)
                .unwrap_or_else(|| panic!("build_chromium_args returned None for {action:?}"));
            assert_eq!(msg.method, expected_method, "wrong method for {action:?}");
        }
    }

    /// web.snapshot must capture shadow DOM + iframe content (`pierce:true`),
    /// matching web.navigate (shim STEP 5, already pierce:true) so the two DOM
    /// captures hash a comparable node set. Locks the unified contract against
    /// silent regression — see specs/2026-06-09-unify-pierce-setting/plan.md.
    #[test]
    fn build_chromium_args_snapshot_emits_pierce_true() {
        let action = Action::WebSnapshot {
            session_id: s("sess-1"),
        };
        let msg = decode_cdp(&action).expect("Some");
        assert_eq!(msg.method, "DOM.getDocument");
        match params_get(&msg, "pierce").expect("pierce param") {
            ciborium::value::Value::Bool(b) => {
                assert!(*b, "snapshot DOM.getDocument must use pierce:true")
            }
            other => panic!("pierce not a Bool: {other:?}"),
        }
    }

    /// evaluate carries the user expression verbatim.
    #[test]
    fn build_chromium_args_evaluate_emits_runtime_evaluate_with_expression() {
        let action = Action::WebEvaluate {
            session_id: s("sess"),
            expression: s("1+1"),
        };
        let msg = decode_cdp(&action).expect("Some");
        assert_eq!(msg.method, "Runtime.evaluate");
        assert_eq!(expr_of(&msg), "1+1");
    }

    // ---- web.scroll viewport-targeting JS (build_scroll_expression) ----

    /// `--selector body` must target the document scrolling box
    /// (`document.scrollingElement`), NOT `body.scrollBy` (a no-op on standard
    /// pages). The `el === document.body` guard routes body → scrollingElement.
    #[test]
    fn build_scroll_expression_targets_scrolling_element_for_body() {
        let js = build_scroll_expression(&Some(s("body")), 0, 1400);
        assert!(
            js.contains("document.scrollingElement"),
            "body scroll must target scrollingElement: {js}"
        );
        assert!(
            js.contains("document.body"),
            "must guard el === document.body: {js}"
        );
        // returns the post-scroll viewport position
        assert!(js.contains("window.scrollX") && js.contains("window.scrollY"));
        assert!(js.contains("scrollBy(0,1400)"));
    }

    /// No selector (the new default) → `null`, so the box resolves to
    /// `document.scrollingElement` and the page viewport scrolls.
    #[test]
    fn build_scroll_expression_null_selector_falls_back_to_scrolling_element() {
        let js = build_scroll_expression(&None, 0, 800);
        // selector is embedded as the JS literal `null`
        assert!(
            js.contains("const el=null?"),
            "absent selector must embed as null: {js}"
        );
        assert!(js.contains("document.scrollingElement"));
        assert!(js.contains("scrollBy(0,800)"));
    }

    /// A real CSS selector is embedded as its querySelector argument; the box
    /// falls through to the resolved element (not scrollingElement).
    #[test]
    fn build_scroll_expression_uses_resolved_selector_for_real_css() {
        let js = build_scroll_expression(&Some(s(".feed")), 10, 0);
        assert!(
            js.contains(r#"document.querySelector(".feed")"#),
            "must query the real selector: {js}"
        );
        assert!(js.contains("scrollBy(10,0)"));
    }

    /// Injection guard: a selector containing `"` is JSON-escaped via
    /// `serde_json::to_string`, so it cannot break out of the JS string literal.
    #[test]
    fn build_scroll_expression_quotes_selector_with_double_quote() {
        let js = build_scroll_expression(&Some(s(r#"a"b"#)), 0, 0);
        // exact escaped form: the JS string literal "a\"b"
        assert!(
            js.contains(r#""a\"b""#),
            "double-quote selector must be JSON-escaped: {js}"
        );
        // and never the unescaped break-out
        assert!(!js.contains(r#"querySelector(a"b)"#));
    }

    /// scroll_result promotion: a valid `{x,y}` value moves into `scroll_result`
    /// and `return_value_json` is cleared (single source of truth — anti-drift).
    #[test]
    fn promote_scroll_result_moves_value_and_clears_return_value_json() {
        let mut r = profile_restricted_evaluate_receipt(1, "sess", "p");
        r.return_value_json = Some(r#"{"x":0,"y":1400}"#.to_string());
        promote_scroll_result(&mut r);
        assert_eq!(
            r.return_value_json, None,
            "return_value_json must be cleared"
        );
        let sr = r.scroll_result.expect("scroll_result populated");
        assert_eq!(sr["y"], 1400);
        assert_eq!(sr["x"], 0);
    }

    /// Robustness: an unparseable value is NOT silently dropped — `return_value_json`
    /// is preserved and `scroll_result` stays None. (Cannot happen for canonical
    /// host JSON, but guards against silent data loss.)
    #[test]
    fn promote_scroll_result_preserves_unparseable_value() {
        let mut r = profile_restricted_evaluate_receipt(1, "sess", "p");
        r.return_value_json = Some("not json{".to_string());
        promote_scroll_result(&mut r);
        assert!(r.scroll_result.is_none());
        assert_eq!(r.return_value_json.as_deref(), Some("not json{"));
    }

    /// cdp-trusted-input: web.click is ALWAYS trusted now — intercepted host-side
    /// (CDP Input.dispatchMouseEvent at the element hit point), so
    /// build_chromium_args returns None (no guest Runtime.evaluate click).
    #[test]
    fn build_chromium_args_click_is_host_side_returns_none() {
        let action = Action::WebClick {
            session_id: s("sess"),
            selector: s("a"),
            until: None,
        };
        assert!(
            decode_cdp(&action).is_none(),
            "web.click is host-side (trusted Input.dispatchMouseEvent) → expected None"
        );
    }

    /// cdp-trusted-input regression: the host-side input receipt MUST carry an
    /// `action_hash` (the run_e2e.sh CLI-surface test asserts every interaction
    /// receipt has one) AND that hash must be SESSION-INDEPENDENT so replay stays
    /// equal across sessions.
    #[test]
    fn build_input_dispatch_receipt_sets_session_independent_action_hash() {
        use crate::wire_receipts::build_input_dispatch_receipt;
        use loom_host::shim_manager::InputDispatchOutcome;
        let a_sess_a = Action::WebClick {
            session_id: s("sess-A"),
            selector: s("#ok-button"),
            until: None,
        };
        let a_sess_b = Action::WebClick {
            session_id: s("sess-B"),
            selector: s("#ok-button"),
            until: None,
        };
        let r_a = build_input_dispatch_receipt(1, "sess-A", &a_sess_a, InputDispatchOutcome::Ok);
        let r_b = build_input_dispatch_receipt(2, "sess-B", &a_sess_b, InputDispatchOutcome::Ok);
        assert!(
            r_a.action_hash.is_some(),
            "host-side click receipt must carry action_hash (e2e CLI-surface contract)"
        );
        assert!(
            r_a.outcome_hash.is_some(),
            "constant dispatch-marker expected"
        );
        assert_eq!(
            r_a.action_hash, r_b.action_hash,
            "action_hash must be session-independent (replay-equal across sessions)"
        );
        // Different selector → different action_hash.
        let a_other = Action::WebClick {
            session_id: s("sess-A"),
            selector: s("#other"),
            until: None,
        };
        let r_other = build_input_dispatch_receipt(3, "sess-A", &a_other, InputDispatchOutcome::Ok);
        assert_ne!(r_a.action_hash, r_other.action_hash);
    }

    /// NFR-DET-01: `DispatchedAckPending` — a click whose cross-origin navigation
    /// swallowed the mouse-dispatch ack (the input WAS performed) — MUST hash
    /// IDENTICALLY to `Ok`. Whether the record-time ack arrived or was lost to a
    /// renderer swap must NEVER perturb the manifest hash chain, or replay would
    /// diverge. The degraded readiness rides on `settle_outcome`, off the chain.
    #[test]
    fn dispatched_ack_pending_hashes_identically_to_ok() {
        use crate::wire_receipts::build_input_dispatch_receipt;
        use loom_host::shim_manager::InputDispatchOutcome;
        let action = Action::WebClick {
            session_id: s("sess"),
            selector: s("#go"),
            until: None,
        };
        let r_ok = build_input_dispatch_receipt(1, "sess", &action, InputDispatchOutcome::Ok);
        let r_pending = build_input_dispatch_receipt(
            1,
            "sess",
            &action,
            InputDispatchOutcome::DispatchedAckPending,
        );
        assert_eq!(
            r_ok.outcome_hash, r_pending.outcome_hash,
            "DispatchedAckPending must carry the SAME constant dispatch marker as Ok \
             (ack timing off the hash chain — replay-equal, NFR-DET-01)"
        );
        assert_eq!(
            r_ok.action_hash, r_pending.action_hash,
            "action_hash must be identical regardless of ack timing"
        );
    }

    /// `web.type` fill refusals are typed errors with FIXED messages: the typed
    /// text (F7 — it can be a password) never appears, and nothing is hashed as a
    /// dispatched outcome.
    #[test]
    fn fill_refusals_are_typed_errors_without_the_typed_text() {
        use crate::wire_receipts::build_input_dispatch_receipt;
        use loom_host::shim_manager::{InputDispatchOutcome, SetValueType};
        use loom_rpc::host_service_adapter::host_service_adapter::ReceiptStatus;
        let action = Action::WebType {
            session_id: s("sess"),
            selector: s("role=textbox[name=\"First day\"]"),
            text: s("12/31/2036-secret"),
            mode: None,
            until: None,
        };
        let cases = [
            (
                InputDispatchOutcome::MalformedValue(SetValueType::Date),
                "malformed_value",
                "yyyy-mm-dd",
            ),
            (
                InputDispatchOutcome::NotEditable,
                "not_editable",
                "disabled or readonly",
            ),
        ];
        for (outcome, kind, says) in cases {
            let r = build_input_dispatch_receipt(1, "sess", &action, outcome);
            assert!(matches!(r.status, ReceiptStatus::Error), "{kind}");
            let err = r.error.as_ref().expect("an error receipt");
            assert_eq!(err.kind, kind);
            let message = err.detail.as_ref().unwrap()["message"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(message.contains(says), "{kind}: {message}");
            assert!(
                !message.contains("secret"),
                "{kind} must not echo the typed text"
            );
            assert_eq!(r.outcome_hash, None, "{kind} is not a dispatched input");
            assert!(
                r.action_hash.is_some(),
                "{kind} still carries the action_hash"
            );
        }
    }

    /// The trusted-input DISPATCH budget is bounded inside the effective deadline
    /// AND stays above real CDP ack latency (tens of ms). The load-bearing
    /// regression guard: a common tight deadline (1–3s) must NOT collapse the
    /// dispatch recv to ~1ms — that loses the mouse-ack race on real Chrome and
    /// reports a normal click as `click_failed`. (It must also never be 0, which
    /// would re-collapse the recv to the ~30s floor.)
    #[test]
    fn interaction_dispatch_budget_leaves_room_for_a_real_ack() {
        use crate::wasm_bridge::interaction_dispatch_budget_ms;
        // No deadline (and the loom `Some(0)` == no-deadline convention) → base.
        assert_eq!(interaction_dispatch_budget_ms(None), 10_000);
        assert_eq!(interaction_dispatch_budget_ms(Some(0)), 10_000);
        // A common tight deadline must leave a REALISTIC ack budget (≫ tens of ms),
        // not collapse toward 1ms — the ship-review regression. A real CDP ack is
        // several to tens of ms, so require comfortably ≥ several hundred ms.
        for dl in [3_000_u64, 5_000, 10_000] {
            let b = interaction_dispatch_budget_ms(Some(dl));
            assert!(
                b >= 500,
                "deadline_ms={dl} gave dispatch budget {b}ms — too tight for a real \
                 CDP ack; a normal click would fail its mouse-ack race"
            );
            assert!(b <= dl, "dispatch budget must stay inside the deadline");
        }
        // Bounded by the base for large deadlines.
        assert!(interaction_dispatch_budget_ms(Some(50_000)) <= 10_000);
        // Never 0 (would re-collapse the dispatch recv to the ~30s floor).
        assert!(interaction_dispatch_budget_ms(Some(1)) >= 1);
    }

    /// interactive-settle-bounded: the interaction settle budget is ALWAYS bounded
    /// strictly inside the caller's `deadline_ms` (minus headroom) — this is the
    /// "a completed click never surfaces an rpc timeout" guarantee at the budget
    /// level. Also: no deadline → the fixed navigate-style default (10s), itself
    /// well inside the 15s tools/call deadline.
    #[test]
    fn interaction_settle_budget_is_bounded_inside_deadline() {
        use crate::wasm_bridge::interaction_settle_budget_ms;
        // No deadline → fixed default, comfortably under the 15s RPC deadline.
        assert_eq!(interaction_settle_budget_ms(None, 0), 10_000);
        // loom convention: `Some(0)` ALSO means "no deadline" → full base budget,
        // NOT a 0ms deadline (ship-council FND-0005/0007).
        assert_eq!(interaction_settle_budget_ms(Some(0), 0), 10_000);
        // Typical loom-mcp deadline (15s): budget stays under it.
        assert!(interaction_settle_budget_ms(Some(15_000), 0) < 15_000);
        // Caller-tighter-than-default deadline: budget clamps BELOW it.
        assert!(
            interaction_settle_budget_ms(Some(5_000), 0) < 5_000,
            "budget must clamp below a tight deadline so the receipt beats it"
        );
        // Elapsed input-dispatch time is subtracted from the remaining budget.
        // Headroom is now 3s (reserves room for the shim's post-settle resume
        // cleanup, click-cross-origin-until ship FND-0001), so a 5s deadline with
        // 3s already spent dispatching leaves 0 (5000-3000-3000 saturates) — out
        // of time, an immediate typed timeout, NOT a floored window that would
        // overrun the deadline (ship FND-0009).
        assert_eq!(interaction_settle_budget_ms(Some(5_000), 3_000), 0);
        // Pathologically tiny deadline: no lower floor — a deadline smaller than
        // the headroom yields 0 (return fast) rather than being floored UP past
        // the caller's own deadline (ship FND-0009).
        assert_eq!(interaction_settle_budget_ms(Some(200), 0), 0);
        // Deadline already blown by a slow dispatch: 0, never a floored window.
        assert_eq!(interaction_settle_budget_ms(Some(1_000), 5_000), 0);
        // A deadline with room to spare past the headroom yields a real window.
        assert_eq!(interaction_settle_budget_ms(Some(6_000), 0), 3_000); // 6000-3000
    }

    /// interactive-settle-bounded determinism (NFR-DET-01): folding the settle
    /// verdict onto an input receipt stamps ONLY the observational
    /// `settle_until`/`settle_outcome` fields and leaves `outcome_hash` /
    /// `action_hash` byte-identical — so replay stays structural.
    #[test]
    fn stamp_settle_outcome_leaves_hashes_untouched() {
        use crate::wire_receipts::{build_input_dispatch_receipt, stamp_settle_outcome};
        use loom_host::shim_manager::InputDispatchOutcome;
        let action = Action::WebClick {
            session_id: s("sess"),
            selector: s("#go"),
            until: Some(s("settled")),
        };
        let mut receipt =
            build_input_dispatch_receipt(7, "sess", &action, InputDispatchOutcome::Ok);
        let hash_before = receipt.outcome_hash.clone();
        let action_hash_before = receipt.action_hash.clone();
        assert!(receipt.settle_outcome.is_none(), "no settle stamped yet");

        let verdict = loom_shared::navigate_outcome::WaitOutcome {
            settle_until: s("settled"),
            settle_outcome: s("dom_unstable"),
            settle_ms: 10_000,
            network_count_at_settle: 1,
        };
        stamp_settle_outcome(&mut receipt, &verdict);

        assert_eq!(receipt.settle_until.as_deref(), Some("settled"));
        assert_eq!(receipt.settle_outcome.as_deref(), Some("dom_unstable"));
        assert_eq!(
            receipt.outcome_hash, hash_before,
            "settle verdict must NOT change the constant outcome_hash marker"
        );
        assert_eq!(
            receipt.action_hash, action_hash_before,
            "settle verdict must NOT change the session-independent action_hash"
        );
    }

    /// web.wait is now host-intercepted (poll `host.wait` → `send_wait`, reusing the
    /// locator-grammar resolver), so build_chromium_args returns None — the guard
    /// against regressing back to the raw `querySelector(sel)` guest envelope that
    /// threw `js_throw` on `text=`/`role=` locators (the reported bug).
    #[test]
    fn build_chromium_args_wait_is_host_side_returns_none() {
        let action = Action::WebWait {
            session_id: s("sess"),
            selector: s("text=Ready 1"),
            timeout_ms: Some(5000),
        };
        assert!(
            decode_cdp(&action).is_none(),
            "web.wait is host-side (polled locator resolution) → expected None"
        );
    }

    /// The host-side `web.wait` receipt mirrors the click contract: a `Resolved`
    /// wait carries a constant `outcome_hash` marker + a SESSION-INDEPENDENT
    /// `action_hash` (replay-equal), and a `PredicateFalse` wait surfaces the typed
    /// `wait_predicate_false` error kind. `timeout_ms` is excluded from the hash.
    #[test]
    fn build_wait_receipt_is_session_independent_and_typed() {
        use crate::wire_receipts::build_wait_receipt;
        use loom_host::shim_manager::WaitResolveOutcome;
        let a_sess_a = Action::WebWait {
            session_id: s("sess-A"),
            selector: s("text=Ready 1"),
            timeout_ms: Some(5000),
        };
        let a_sess_b = Action::WebWait {
            session_id: s("sess-B"),
            selector: s("text=Ready 1"),
            timeout_ms: Some(9999), // different timeout → must NOT change the hash
        };
        let r_a = build_wait_receipt(1, "sess-A", &a_sess_a, WaitResolveOutcome::Resolved);
        let r_b = build_wait_receipt(2, "sess-B", &a_sess_b, WaitResolveOutcome::Resolved);
        assert!(
            r_a.action_hash.is_some(),
            "host-side wait receipt must carry action_hash"
        );
        assert!(
            r_a.outcome_hash.is_some(),
            "resolved wait must stamp the constant dispatch marker"
        );
        assert_eq!(
            r_a.action_hash, r_b.action_hash,
            "action_hash must be session- and timeout-independent (replay-equal)"
        );
        // Different selector → different action_hash.
        let a_other = Action::WebWait {
            session_id: s("sess-A"),
            selector: s("text=Other"),
            timeout_ms: Some(5000),
        };
        let r_other = build_wait_receipt(3, "sess-A", &a_other, WaitResolveOutcome::Resolved);
        assert_ne!(r_a.action_hash, r_other.action_hash);

        // PredicateFalse → typed wait_predicate_false error receipt, no outcome marker.
        let r_to = build_wait_receipt(4, "sess-A", &a_sess_a, WaitResolveOutcome::PredicateFalse);
        let err = r_to.error.expect("timeout must produce an error receipt");
        assert_eq!(err.kind, "wait_predicate_false");
    }

    /// `mode:"value"` sets value via the framework-aware native setter (so
    /// React/Vue/Angular trackers see the change) AND dispatches input/change
    /// events. (The default `fill` mode is host-side CDP Input.insertText and
    /// builds no Runtime.evaluate args — covered by the host-side fill tests.)
    #[test]
    fn build_chromium_args_type_emits_runtime_evaluate_setting_value_and_dispatching_input_change()
    {
        let action = Action::WebType {
            session_id: s("sess"),
            selector: s("input"),
            text: s("hello"),
            mode: Some(s("value")),
            until: None,
        };
        let msg = decode_cdp(&action).expect("Some");
        assert_eq!(msg.method, "Runtime.evaluate");
        let expr = expr_of(&msg);
        // Framework-aware: must call the prototype's value setter, not assign
        // `.value =` directly (which bypasses React's tracker).
        assert!(
            expr.contains("setter.call(el,"),
            "expected setter.call(el, ...) in {expr}"
        );
        assert!(
            expr.contains("HTMLInputElement.prototype"),
            "expected HTMLInputElement.prototype in {expr}"
        );
        assert!(
            !expr.contains(";el.value="),
            "regression: direct el.value= bypasses React tracker, in {expr}"
        );
        assert!(
            expr.contains("new Event('input'"),
            "expected input event in {expr}"
        );
        assert!(
            expr.contains("new Event('change'"),
            "expected change event in {expr}"
        );
    }

    /// screenshot uses Page.captureScreenshot { format: "png" }.
    #[test]
    fn build_chromium_args_screenshot_emits_page_capture_screenshot_png() {
        let action = Action::WebScreenshot {
            session_id: s("sess"),
            selector: None,
        };
        let msg = decode_cdp(&action).expect("Some");
        assert_eq!(msg.method, "Page.captureScreenshot");
        match params_get(&msg, "format").expect("format param") {
            ciborium::value::Value::Text(t) => assert_eq!(t, "png"),
            other => panic!("format not text: {other:?}"),
        }
    }

    // === build_wire_receipt_error: wire shape ===

    #[test]
    fn build_wire_receipt_error_shim_failure_with_typed_http_status_detail() {
        let detail =
            r#"{"kind":"http_status","url":"http://fake.test/status/404","status_code":404}"#;
        let err = build_wire_receipt_error("shim-failure", Some(detail));
        assert_eq!(err.kind, "http_status");
        let d = err.detail.as_ref().expect("detail must be present");
        assert_eq!(
            d.get("url").and_then(|v| v.as_str()),
            Some("http://fake.test/status/404")
        );
        assert_eq!(d.get("status_code").and_then(|v| v.as_u64()), Some(404));
        // `kind` must NOT be in detail — it's been hoisted to the wire kind field.
        assert!(
            d.get("kind").is_none(),
            "kind should be hoisted, not in detail"
        );
    }

    #[test]
    fn build_wire_receipt_error_shim_failure_with_typed_dns_failure_detail() {
        let detail = r#"{"kind":"dns_failure","url":"http://fake.test/error/x","chromium_error":"net::ERR_NAME_NOT_RESOLVED"}"#;
        let err = build_wire_receipt_error("shim-failure", Some(detail));
        assert_eq!(err.kind, "dns_failure");
        let d = err.detail.as_ref().expect("detail must be present");
        assert_eq!(
            d.get("chromium_error").and_then(|v| v.as_str()),
            Some("net::ERR_NAME_NOT_RESOLVED")
        );
    }

    #[test]
    fn build_wire_receipt_error_untyped_shim_failure_falls_back_to_message() {
        // Plain-string shim-failure (not structured JSON) — the raw string
        // becomes detail.message; kind keeps the raw error_code.
        let err = build_wire_receipt_error("shim-failure", Some("chromium subprocess died"));
        assert_eq!(err.kind, "shim-failure");
        let d = err.detail.as_ref().expect("detail must be present");
        assert_eq!(
            d.get("message").and_then(|v| v.as_str()),
            Some("chromium subprocess died")
        );
    }

    #[test]
    fn build_wire_receipt_error_non_shim_failure_uses_code_as_kind() {
        let err = build_wire_receipt_error("budget-exceeded", Some("navigate exceeded 30s"));
        assert_eq!(err.kind, "budget-exceeded");
        let d = err.detail.as_ref().expect("detail must be present");
        assert_eq!(
            d.get("message").and_then(|v| v.as_str()),
            Some("navigate exceeded 30s")
        );
    }

    #[test]
    fn build_wire_receipt_error_empty_details_yields_no_detail() {
        let err = build_wire_receipt_error("internal", None);
        assert_eq!(err.kind, "internal");
        assert!(err.detail.is_none());
        let err2 = build_wire_receipt_error("internal", Some(""));
        assert!(err2.detail.is_none());
    }

    /// Security: selector strings containing JS metacharacters must be
    /// JSON-escaped wherever they're interpolated into Runtime.evaluate JS.
    /// cdp-trusted-input: web.click is now host-side (the selector flows as a
    /// CBOR CDP `DOM.querySelector` param — no JS-injection surface), so this
    /// pins the remaining JS-interpolating path: web.type in `value` mode.
    #[test]
    fn build_chromium_args_value_type_json_escapes_selector_with_double_quote() {
        // selector contains a literal double-quote character: a[id="x']
        let selector = "a[id=\"x']".to_string();
        let action = Action::WebType {
            session_id: s("sess"),
            selector: selector.clone(),
            text: s("v"),
            mode: Some(s("value")),
            until: None,
        };
        let msg = decode_cdp(&action).expect("Some");
        let expr = expr_of(&msg);
        // Must contain the JSON-escaped form: "a[id=\"x']"
        assert!(
            expr.contains("\"a[id=\\\"x']\""),
            "selector not JSON-escaped; expr was: {expr}"
        );
        // Must NOT contain a raw unescaped double-quote inside the literal that
        // would have closed the JS string early.
        // Validate by parsing the expression: it should still be a syntactically
        // closeable JS source — at minimum, count of unescaped double-quotes
        // should be even (open+close pairs).
        let mut escaped = false;
        let mut quotes = 0usize;
        for ch in expr.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => quotes += 1,
                _ => {}
            }
        }
        assert_eq!(
            quotes % 2,
            0,
            "odd number of unescaped quotes in expr: {expr}"
        );
    }

    /// cdp-trusted-input: the DEFAULT (`mode:None`) and explicit `mode:"fill"`
    /// are dispatched host-side (CDP Input.insertText), so `build_chromium_args`
    /// builds NO Runtime.evaluate args for them — proving the default flip is
    /// routed away from the WASM-guest value path. `mode:"keystrokes"` is also
    /// host-side; only `value` (and unknown → value) builds guest args.
    #[test]
    fn build_chromium_args_type_default_and_fill_are_host_side_no_guest_args() {
        for mode in [None, Some(s("fill")), Some(s("keystrokes"))] {
            let action = Action::WebType {
                session_id: s("sess"),
                selector: s("input"),
                text: s("hello"),
                mode: mode.clone(),
                until: None,
            };
            assert!(
                decode_cdp(&action).is_none(),
                "web.type mode {mode:?} must be host-side intercepted (no Runtime.evaluate args)"
            );
        }
        // value (and an unknown string) DO build guest args.
        for mode in [Some(s("value")), Some(s("totally-unknown"))] {
            let action = Action::WebType {
                session_id: s("sess"),
                selector: s("input"),
                text: s("hello"),
                mode,
                until: None,
            };
            let msg = decode_cdp(&action).expect("value/unknown mode builds guest args");
            assert_eq!(msg.method, "Runtime.evaluate");
        }
    }

    /// The single-source-of-truth mode classifier (decisions.md D8).
    #[test]
    fn classify_web_type_mode_maps_modes_to_dispatch_paths() {
        use crate::wire_receipts::{classify_web_type_mode, WebTypeDispatch};
        assert_eq!(classify_web_type_mode(None), WebTypeDispatch::Fill);
        assert_eq!(classify_web_type_mode(Some("fill")), WebTypeDispatch::Fill);
        assert_eq!(
            classify_web_type_mode(Some("keystrokes")),
            WebTypeDispatch::Keystrokes
        );
        assert_eq!(
            classify_web_type_mode(Some("value")),
            WebTypeDispatch::ValueGuest
        );
        // Unknown strings fall back to value (back-compat — no error).
        assert_eq!(
            classify_web_type_mode(Some("nope")),
            WebTypeDispatch::ValueGuest
        );
    }

    // ─── Tests for build_navigate_wire_receipt ─────────────────────────
    //
    // These pin the production wire-receipt construction path that
    // `WasmHostBridge::dispatch_action_blocking` invokes. Required because
    // every other test for the daemon's dispatch path stubs the trait
    // (`loom-rpc/tests/...`) or stops at the shim layer
    // (`loom-host/tests/integration_navigate_tier2_e2e.rs`); without
    // these, the `Receipt` construction + JSON-decode + capture-policy
    // arms would ship with no direct test coverage.

    use loom_core::receipt_builder::receipt_builder::NetworkSummary;
    use loom_host::receipt_marshaller::{ReceiptBuilder, ReceiptStatus as HostStatus};
    use loom_shared::navigate_outcome::{LoomNetworkEvent, ShimConsoleLine};

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

    ///..04: default profile carries every brief-listed key
    /// when the upstream JSON blobs are well-formed. Tests the actual
    /// production decode path.
    #[test]
    fn build_navigate_wire_receipt_decodes_all_three_json_blobs_under_default() {
        let builder = navigate_builder_with_all_blobs();
        let r = build_navigate_wire_receipt(&builder, "S1", None);

        assert_eq!(r.action_id, 11);
        assert_eq!(r.session_id, "S1");
        assert_eq!(r.url.as_deref(), Some("https://example.com/"));
        assert_eq!(r.status_code, Some(200));
        assert_eq!(r.dom_snapshot_hash.as_ref().map(String::len), Some(64));
        assert_eq!(r.screenshot_after_hash.as_ref().map(String::len), Some(64));
        assert_eq!(r.console_lines.len(), 1);
        assert_eq!(r.console_lines[0].level, "info");
        let s = r.network_summary.as_ref().expect("network_summary present");
        assert_eq!(s.total_count, 2);
        assert_eq!(s.total_bytes, 5120);
        assert_eq!(s.error_count, 0);
        assert_eq!(r.side_effects.len(), 2);
        assert_eq!(r.side_effects[0]["method"], "GET");
        assert_eq!(r.side_effects[0]["status"], 200);
    }

    /// web.get_cookies surfacing: a builder carrying `get_cookies_result`
    /// (set host-side from the decoded Network.getCookies response) must land
    /// on the wire receipt as a parsed JSON array with RAW values (D7).
    #[test]
    fn build_navigate_wire_receipt_surfaces_get_cookies_result() {
        let builder = ReceiptBuilder {
            action_id: 7,
            status: HostStatus::Ok,
            action_hash: "aa".repeat(32),
            outcome_hash: "cc".repeat(32),
            emitted_at_ms: 1_714_074_336_000,
            get_cookies_result: Some(
                r#"[{"name":"NID","value":"raw-token","domain":".google.com","httpOnly":true}]"#
                    .to_string(),
            ),
            ..Default::default()
        };
        let r = build_navigate_wire_receipt(&builder, "S1", None);
        let cookies = r.get_cookies_result.expect("get_cookies_result surfaced");
        assert!(cookies.is_array());
        assert_eq!(cookies[0]["name"], "NID");
        // RAW value preserved on the operator-facing wire receipt (D7).
        assert_eq!(cookies[0]["value"], "raw-token");
        assert_eq!(cookies[0]["httpOnly"], true);
    }

    /// Non-cookie verbs leave `get_cookies_result` absent (no regression).
    #[test]
    fn build_navigate_wire_receipt_omits_get_cookies_for_non_cookie_verbs() {
        let builder = navigate_builder_with_all_blobs();
        let r = build_navigate_wire_receipt(&builder, "S1", None);
        assert!(r.get_cookies_result.is_none());
    }

    /// --capture-policy minimal strips tier-2 fields
    /// at the wire boundary. This is the test that actually exercises
    /// the `apply_capture_profile_to_wire(...)` invocation in the
    /// production code path.
    #[test]
    fn build_navigate_wire_receipt_minimal_strips_per_brief() {
        let builder = navigate_builder_with_all_blobs();
        let r = build_navigate_wire_receipt(&builder, "S1", Some("minimal"));

        // Identity + brief-listed survivors:
        assert_eq!(r.action_id, 11);
        assert_eq!(r.session_id, "S1");
        assert_eq!(r.url.as_deref(), Some("https://example.com/"));
        assert_eq!(r.status_code, Some(200));

        // Stripped:
        assert!(r.dom_snapshot_hash.is_none());
        assert!(r.screenshot_after_hash.is_none());
        assert!(r.console_lines.is_empty());
        assert!(r.network_summary.is_none());
        assert!(r.network_count.is_none());
        assert!(r.console_count.is_none());
        assert!(r.final_url.is_none());
        assert!(r.title.is_none());
        assert!(r.side_effects.is_empty());
        assert!(r.action_hash.is_none());
        assert!(r.outcome_hash.is_none());
        assert!(r.emitted_at_ms.is_none());
    }

    /// network_entries side-channel surfaces inline AND leaves the
    /// existing hashed/aggregate fields (network_count, side_effects,
    /// network_summary) byte-identical (backward-compat: separate path).
    #[test]
    fn build_navigate_wire_receipt_surfaces_inline_network_entries() {
        let mut builder = navigate_builder_with_all_blobs();
        let entries = vec![loom_shared::navigate_outcome::LoomNetworkEntry {
            url: "https://app.test/api/thing".into(),
            method: "GET".into(),
            status: 200,
            resource_type: "XHR".into(),
            from_cache: false,
            request_id: "R-1".into(),
            ts_ms: 1_700_000_000_000,
        }];
        builder.navigate_network_entries_json = Some(serde_json::to_vec(&entries).unwrap());
        let r = build_navigate_wire_receipt(&builder, "S1", None);

        // network_entries surfaced.
        assert_eq!(r.network_entries.len(), 1);
        assert_eq!(r.network_entries[0]["method"], "GET");
        assert_eq!(r.network_entries[0]["status"], 200);
        assert_eq!(r.network_entries[0]["resource_type"], "XHR");
        assert!(r.network_entries_blob_ref.is_none());

        // Backward-compat: the existing fields are untouched by the new path.
        assert_eq!(r.network_count, Some(2));
        assert_eq!(r.side_effects.len(), 2);
        assert!(r.network_summary.is_some());
    }

    /// When the host offloaded the list, the wire receipt carries the
    /// blob_ref (sha256) and an EMPTY inline list — the inline-XOR-blob
    /// discriminator, mirroring return_value_blob_ref.
    #[test]
    fn build_navigate_wire_receipt_surfaces_network_entries_blob_ref() {
        let mut builder = navigate_builder_with_all_blobs();
        builder.navigate_network_entries_json = None;
        builder.navigate_network_entries_blob_ref = Some(loom_core::content_store::ContentRef {
            sha256: "c".repeat(64),
            size_bytes: 70_000,
        });
        builder.navigate_network_entries_truncated = Some(false);
        let r = build_navigate_wire_receipt(&builder, "S1", None);

        assert!(r.network_entries.is_empty());
        assert_eq!(
            r.network_entries_blob_ref.as_ref().map(String::len),
            Some(64)
        );
    }

    /// --capture-policy minimal strips the observational network_entries.
    #[test]
    fn build_navigate_wire_receipt_minimal_strips_network_entries() {
        let mut builder = navigate_builder_with_all_blobs();
        let entries = vec![loom_shared::navigate_outcome::LoomNetworkEntry {
            url: "https://app.test/x".into(),
            method: "GET".into(),
            status: 200,
            resource_type: "Fetch".into(),
            from_cache: false,
            request_id: "R-1".into(),
            ts_ms: 1,
        }];
        builder.navigate_network_entries_json = Some(serde_json::to_vec(&entries).unwrap());
        builder.navigate_network_entries_truncated = Some(true);
        let r = build_navigate_wire_receipt(&builder, "S1", Some("minimal"));
        assert!(r.network_entries.is_empty());
        assert!(r.network_entries_blob_ref.is_none());
        assert!(r.network_entries_truncated.is_none());
    }

    /// `capture_policy_str = Some("default")` and `Some("full")` are
    /// no-ops on the wire today; Full will gain `dom_full_text`
    /// semantics in a future PR.
    #[test]
    fn build_navigate_wire_receipt_default_and_full_are_noops() {
        let builder = navigate_builder_with_all_blobs();
        let none_r = build_navigate_wire_receipt(&builder, "S", None);
        let default_r = build_navigate_wire_receipt(&builder, "S", Some("default"));
        let full_r = build_navigate_wire_receipt(&builder, "S", Some("full"));

        let to_json = |r: &Receipt| serde_json::to_value(r).unwrap();
        assert_eq!(to_json(&none_r), to_json(&default_r));
        assert_eq!(to_json(&none_r), to_json(&full_r));
    }

    /// Decode-failure paths: malformed JSON in any of the three navigate
    /// blobs degrades to empty/None instead of failing the navigate
    /// (observability fields shouldn't trap). This pins the
    /// `tracing::warn` arms.
    #[test]
    fn build_navigate_wire_receipt_degrades_on_malformed_console_lines_json() {
        let mut builder = navigate_builder_with_all_blobs();
        builder.navigate_console_lines_json = Some(b"not valid json".to_vec());
        let r = build_navigate_wire_receipt(&builder, "S", None);
        assert!(
            r.console_lines.is_empty(),
            "must degrade to empty, not panic"
        );
        // Other fields unaffected:
        assert_eq!(r.url.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn build_navigate_wire_receipt_degrades_on_malformed_network_summary_json() {
        let mut builder = navigate_builder_with_all_blobs();
        builder.navigate_network_summary_json = Some(b"{not json".to_vec());
        let r = build_navigate_wire_receipt(&builder, "S", None);
        assert!(
            r.network_summary.is_none(),
            "must degrade to None, not panic"
        );
    }

    #[test]
    fn build_navigate_wire_receipt_degrades_on_malformed_side_effects_json() {
        let mut builder = navigate_builder_with_all_blobs();
        builder.navigate_side_effects_json = Some(b"[not events".to_vec());
        let r = build_navigate_wire_receipt(&builder, "S", None);
        assert!(
            r.side_effects.is_empty(),
            "must degrade to empty, not panic"
        );
    }

    /// Unknown capture-policy string falls back to Default (no-op) —
    /// validation is upstream in `session_validation::validate`. This
    /// ensures a stale / unparseable persisted value doesn't crash
    /// dispatch on an existing session.
    #[test]
    fn build_navigate_wire_receipt_unknown_policy_string_falls_back_to_default() {
        let builder = navigate_builder_with_all_blobs();
        let unknown = build_navigate_wire_receipt(&builder, "S", Some("bogus-profile"));
        let default = build_navigate_wire_receipt(&builder, "S", Some("default"));
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            serde_json::to_value(&default).unwrap(),
            "unknown policy must fall back to Default, not strip / no-op differently"
        );
    }

    // ─── Graceful shutdown signals (SIGINT + SIGTERM) ──────────
    // `kill <daemon.pid>` (SIGTERM — what launchd stop, systemd stop and a
    // plain `kill` deliver) must resolve the shutdown future so
    // `SocketServer::serve` drains instead of the process being hard-killed
    // mid-WAL-append. The tests raise a real signal at this test process:
    // once tokio's handler is installed the default kill disposition is
    // replaced, so the signal is observed by the stream rather than
    // terminating the test binary (workspace tests run --test-threads=1).

    #[tokio::test]
    async fn shutdown_signal_resolves_on_sigterm() {
        let task = tokio::spawn(shutdown_signal());
        // Let the spawned future poll once so the handlers are registered
        // before the signal is raised.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        unsafe {
            libc::kill(std::process::id() as libc::pid_t, libc::SIGTERM);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("shutdown future must resolve on SIGTERM")
            .expect("shutdown future must not panic");
    }

    #[tokio::test]
    async fn shutdown_signal_resolves_on_sigint() {
        let task = tokio::spawn(shutdown_signal());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        unsafe {
            libc::kill(std::process::id() as libc::pid_t, libc::SIGINT);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("shutdown future must resolve on SIGINT")
            .expect("shutdown future must not panic");
    }

    // ─── CoreBridge shim teardown on close AND abort ──────────
    // session.abort previously flipped core state only — the session-bound
    // chromium shim was never torn down, leaking the browser (until orphan
    // GC) and the ShimManager entries (forever). Both lifecycle exits must
    // route through `spawn_shim_teardown`.

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
        let keychain: Arc<dyn loom_core::vault::KeychainAccess> =
            Arc::new(loom_keychain::StubKeychain);
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

    #[tokio::test]
    async fn abort_session_raw_spawns_shim_teardown() {
        let tmp = test_scratch_dir("abort-teardown");
        let bridge = make_bridge(&tmp);
        let sid = create_session_via(&bridge);

        bridge.abort_session_raw(&sid, "test-abort").expect("abort");

        // The teardown task must be in the JoinSet (completed tasks stay in
        // `len()` until joined, and `spawn_shim_teardown` reaps BEFORE the
        // fresh spawn — so exactly this abort's task is observable here).
        assert_eq!(
            bridge.cleanup_tasks.lock().unwrap().len(),
            1,
            "abort must spawn host.shutdown_session into cleanup_tasks \
             (browser + ShimManager entry reclamation)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn close_session_raw_spawns_shim_teardown() {
        let tmp = test_scratch_dir("close-teardown");
        let bridge = make_bridge(&tmp);
        let sid = create_session_via(&bridge);

        bridge.close_session_raw(&sid).expect("close");

        assert_eq!(
            bridge.cleanup_tasks.lock().unwrap().len(),
            1,
            "close must spawn host.shutdown_session into cleanup_tasks"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Acceptance (typed-capacity-errors): saturating the cap yields the
    /// typed `session_cap_exceeded` rejection with `{active, cap, hint}`
    /// context (never the opaque internal catch-all); closing one session
    /// frees a slot and create succeeds again.
    // `max_concurrent_sessions()` caches `LOOM_MAX_CONCURRENT_SESSIONS` in a
    // process-wide OnceLock. That makes this test irreducibly process-global:
    // a sibling session test that initializes the cache first would defeat the
    // pin, and this test's pin would cap every sibling. A per-test ENV_LOCK
    // can't fix a *cache* (only a fresh process can), so this test runs ONLY
    // under nextest's process-per-test isolation (`--run-ignored all` in CI).
    // Default `cargo test` skips it (ignored) — keeping the threaded run green
    // without this test polluting the shared cache.
    #[ignore = "process-global env cache (LOOM_MAX_CONCURRENT_SESSIONS OnceLock) — run under nextest --run-ignored, not threaded cargo test"]
    #[tokio::test]
    async fn create_session_raw_cap_hit_is_typed_and_recovers_after_close() {
        // Pin the cap low. Under nextest this is a fresh process, so the OnceLock
        // latches exactly 2; assert that loudly so an accidental un-isolated run
        // (e.g. `cargo test -- --include-ignored` at default parallelism) FAILS
        // visibly instead of silently saturating the wrong cap.
        std::env::set_var("LOOM_MAX_CONCURRENT_SESSIONS", "2");
        let cap = max_concurrent_sessions();
        assert_eq!(
            cap, 2,
            "this test requires process isolation: the LOOM_MAX_CONCURRENT_SESSIONS \
             OnceLock latched {cap}, not 2 — run it via `cargo nextest run --run-ignored all`, \
             not threaded `cargo test --include-ignored`"
        );
        let tmp = test_scratch_dir("cap-typed-error");
        let bridge = make_bridge(&tmp);

        let mut sids = Vec::new();
        for _ in 0..cap {
            sids.push(create_session_via(&bridge));
        }

        let err = bridge
            .create_session_raw(params_safe())
            .expect_err("create beyond the cap must be rejected");
        assert_eq!(
            err.code,
            loom_core::error::LoomErrorCode::SessionCapExceeded,
            "cap rejection must be the typed code, got: {err}"
        );
        assert!(
            err.message.contains(&format!("({cap}/{cap})")),
            "message must carry active/cap: {}",
            err.message
        );
        let ctx = err.context.expect("cap rejection must carry context");
        assert_eq!(ctx["active"].as_u64(), Some(cap as u64));
        assert_eq!(ctx["cap"].as_u64(), Some(cap as u64));
        assert!(
            ctx["hint"]
                .as_str()
                .is_some_and(|h| h.contains("loom session reap")),
            "hint must name the remediation; got: {ctx}"
        );

        // Close one → a slot frees → create succeeds again.
        bridge
            .close_session_raw(&sids[0])
            .expect("close must succeed");
        let _ = bridge
            .create_session_raw(params_safe())
            .expect("create after freeing a slot must succeed");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Refactor guard (cleanup-create-session-params): `create_session_raw` now takes the wire
    /// `CreateSessionParams` struct by value. This asserts every field still threads onto the
    /// created `Session` using DISTINCT non-default values, so a field-swap mis-map — which the
    /// all-defaults call sites elsewhere cannot catch — fails loudly. Two creates cover both
    /// determinism bools for threading AND the `no_blocklist`↔`no_determinism` swap.
    #[tokio::test]
    async fn create_session_raw_threads_param_fields() {
        use loom_core::manifest_writer::SessionId;
        use loom_shared::types::Seed;

        let tmp = test_scratch_dir("threads-param-fields");
        let bridge = make_bridge(&tmp);

        // create A: seed + no_blocklist set, no_determinism clear, explicit profile.
        let (sid_a, _) = bridge
            .create_session_raw(CreateSessionParams {
                profile: "safe".to_string(),
                network_mode: "live".to_string(),
                capture_policy: None,
                seed: Some(99),
                budget: None,
                no_blocklist: true,
                no_determinism: false,
                clock_anchor: None,
                record_screencast: true,
                audio: true,
            })
            .expect("create A");
        let sess_a = bridge
            .core
            .session_manager
            .get(SessionId(sid_a))
            .expect("get session A");
        assert_eq!(sess_a.seed, Seed(99), "seed must thread through the struct");
        assert!(sess_a.no_blocklist, "no_blocklist=true must thread");
        assert!(
            !sess_a.no_determinism,
            "no_determinism=false must thread (catches a no_blocklist↔no_determinism swap)"
        );
        assert_eq!(sess_a.profile, "safe", "profile must thread");
        assert!(
            sess_a.record_screencast,
            "record_screencast=true must thread (field added by the screencast feature merge)"
        );
        assert!(
            sess_a.audio,
            "audio=true must thread (voice-call-io --audio opt-in reaches Session.audio)"
        );

        // create B: mirror — flips both bools, proving no_determinism threads to `true` too.
        let (sid_b, _) = bridge
            .create_session_raw(CreateSessionParams {
                profile: "safe".to_string(),
                network_mode: "live".to_string(),
                capture_policy: None,
                seed: None,
                budget: None,
                no_blocklist: false,
                no_determinism: true,
                clock_anchor: None,
                record_screencast: false,
                audio: false,
            })
            .expect("create B");
        let sess_b = bridge
            .core
            .session_manager
            .get(SessionId(sid_b))
            .expect("get session B");
        assert!(!sess_b.no_blocklist, "no_blocklist=false must thread");
        assert!(sess_b.no_determinism, "no_determinism=true must thread");
        assert!(
            !sess_b.record_screencast,
            "record_screencast=false must thread"
        );
        assert!(!sess_b.audio, "audio=false must thread");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─── parse_args: per-setting precedence (CLI > env > defaults) ──────────
    // --data-root supplies the DEFAULT <root>/daemon.log, but must never
    // clobber an explicitly-set LOOM_LOG_PATH (monitoring tails the env-set
    // path).

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

    #[test]
    fn data_root_flag_derives_log_path_when_log_path_not_explicit() {
        let args = with_parse_args_env(None, || {
            parse_args(&argv(&["loom-daemon", "--data-root", "/srv/loom"]))
        });
        assert_eq!(args.data_root, PathBuf::from("/srv/loom"));
        assert_eq!(args.log_path, PathBuf::from("/srv/loom/daemon.log"));
    }

    #[test]
    fn data_root_flag_does_not_clobber_explicit_loom_log_path() {
        let args = with_parse_args_env(Some("/var/log/custom-loom.log"), || {
            parse_args(&argv(&["loom-daemon", "--data-root", "/srv/loom"]))
        });
        assert_eq!(args.data_root, PathBuf::from("/srv/loom"));
        assert_eq!(
            args.log_path,
            PathBuf::from("/var/log/custom-loom.log"),
            "an explicit LOOM_LOG_PATH must win over --data-root's derived default"
        );
    }

    #[test]
    fn explicit_loom_log_path_alone_overrides_default() {
        let args = with_parse_args_env(Some("/var/log/custom-loom.log"), || {
            parse_args(&argv(&["loom-daemon"]))
        });
        assert_eq!(args.log_path, PathBuf::from("/var/log/custom-loom.log"));
    }

    // ─── A-W8.1 auth artefacts created 0600 atomically ──────────
    // hello.token is the daemon's sole bearer credential. It must be CREATED
    // with 0600 — a write-then-chmod sequence leaves a transient umask-mode
    // window in which any local user can open the token (and keep the fd
    // past the chmod).

    /// Run `f` with the process umask temporarily set to `mask` (tests run
    /// --test-threads=1, so no other thread races the process-global umask).
    #[cfg(unix)]
    fn with_umask<T>(mask: libc::mode_t, f: impl FnOnce() -> T) -> T {
        let old = unsafe { libc::umask(mask) };
        let out = f();
        unsafe { libc::umask(old) };
        out
    }

    #[cfg(unix)]
    #[test]
    fn write_auth_file_0600_creates_with_0600_under_permissive_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_scratch_dir("auth-0600");
        let path = dir.join("hello.token");
        // umask 0: a plain fs::write would create this 0666 — the regression
        // under test is that the file is NEVER creatable looser than 0600.
        with_umask(0, || {
            write_auth_file_0600(&path, b"tok-secret", "hello.token")
        })
        .expect("write_auth_file_0600");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "auth file must be CREATED 0600 (no transient world-readable window)"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"tok-secret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_auth_file_0600_truncates_and_rewrites_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_scratch_dir("auth-0600-rewrite");
        let path = dir.join("hello.token");
        write_auth_file_0600(&path, b"first-token-longer", "hello.token").unwrap();
        // Daemon restart path: same file, new token — must fully replace.
        write_auth_file_0600(&path, b"second", "hello.token").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn abort_session_raw_without_host_still_aborts() {
        let tmp = test_scratch_dir("abort-no-host");
        let bridge = CoreBridge {
            core: make_core_at(&tmp),
            wasm_host: None,
            cleanup_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
        };
        let sid = create_session_via(&bridge);
        bridge
            .abort_session_raw(&sid, "test-abort")
            .expect("abort without a WasmHost must still succeed");
        assert_eq!(bridge.cleanup_tasks.lock().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
