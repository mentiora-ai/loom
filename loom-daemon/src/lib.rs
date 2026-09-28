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
mod bridge_input;
mod bridge_media;
mod cli_args;
mod core_bridge;
mod guest_args;
mod health;
mod inject_payload;
mod media_receipts;
mod navigate_receipt;
mod settle_budget;
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
pub(crate) use guest_args::*;
#[cfg(test)]
pub(crate) use navigate_receipt::*;
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
mod tests;
