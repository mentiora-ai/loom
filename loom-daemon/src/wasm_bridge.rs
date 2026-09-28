//! `WasmHost` → `WasmHostBridge` bridge + host-bridge construction.
//!
//! Split out of `lib.rs` (large-file refactor), unchanged:
//!   * `StubHostBridge` — returns `SurfaceUnavailable` until `loom
//!     postinstall` compiles the WASM modules.
//!   * `WasmBridge` + its `WasmHostBridge` impl — the real dispatch path:
//!     terminal-status reject, the per-session dispatch fence, the
//!     safe-profile evaluate gate, cookie-grant resolution + per-cookie
//!     validation, the upload allow-list gate, the host/shim-side reads
//!     (network_log / start_recording / stop_recording), payload encoding,
//!     and the `host.dispatch` → wire-receipt round trip.
//!   * `build_host_bridge` — resolves surfaces + Chromium and builds the
//!     real `WasmBridge` (or the stub on load failure). Called by `async_main`.
//!
//! Receipt/payload builders live in `crate::wire_receipts`; the dispatch
//! fence + activity guard live in `crate::core_bridge`.

use crate::core_bridge::{acquire_dispatch_slot, ActionActivityGuard};
use crate::guest_args::*;
use crate::navigate_receipt::*;
use crate::wire_receipts::*;
use crate::{map_loom_error, now_epoch_ms, upload_guard};
use loom_core::core_api_facade::CoreApiFacade;
use loom_rpc::host_service_adapter::host_service_adapter::{
    Action, AdapterError as HostAdapterError, Receipt, WasmHostBridge,
};
use std::path::PathBuf;
use std::sync::Arc;

// ─── Bridge: WasmHost → WasmHostBridge ──────────────────────────────────────

/// Stub host bridge — returns `SurfaceUnavailable` for every action
/// dispatch until WASM modules are compiled by `loom postinstall`.
/// Replaced by a real `WasmHost`-backed impl once modules are present.
struct StubHostBridge;

impl WasmHostBridge for StubHostBridge {
    fn dispatch_action_blocking(
        &self,
        _action: Action,
        _deadline_ms: Option<u64>,
    ) -> Result<Receipt, HostAdapterError> {
        use loom_rpc::error_translator::error_translator::LoomErrorCode;
        Err(LoomErrorCode::SurfaceUnavailable)
    }

    // stub bridge means WASM host failed to load (no surfaces).
    // Reporting false here doesn't change behavior — `dispatch_action`
    // already errors with SurfaceUnavailable — but it produces a clearer
    // BrowserNotFound message at session.create when the surfaces dir
    // happens to be empty AND chromium is missing.
    fn has_chromium(&self) -> bool {
        false
    }
}

/// Real bridge wrapping `Arc<loom_host::WasmHost>`. Uses
/// `tokio::task::block_in_place` + `Handle::block_on` so the async
/// dispatch call is safely driven from a sync bridge method.
pub(crate) struct WasmBridge {
    pub(crate) host: Arc<loom_host::WasmHost>,
    pub(crate) core: Arc<CoreApiFacade>,
    /// was a `ShimChromiumConfig` registered at host boot?
    /// Set at `build_host_bridge` time from the resolver's outcome.
    pub(crate) has_chromium: bool,
    /// Allow-list root for `web.set_input_files` (from `LOOM_UPLOAD_ROOT`).
    /// `None` → uploads fail closed. Daemon-global (all sessions), per
    /// plan-council FND#4 — not threaded per-session.
    pub(crate) upload_root: Option<PathBuf>,
}

impl WasmHostBridge for WasmBridge {
    fn dispatch_action_blocking(
        &self,
        action: Action,
        deadline_ms: Option<u64>,
    ) -> Result<Receipt, HostAdapterError> {
        use loom_host::session_executor::{Action as HostAction, ActionOutcome, SessionHandle};
        use loom_rpc::error_translator::error_translator::LoomErrorCode;

        // Owned copy so the borrow doesn't outlive the move-out of
        // `action` further down (v0.9.7 follow-up rewrites action
        // mid-dispatch for cookie grant resolution).
        let session_id_str_owned = action_session_id(&action).to_string();
        let session_id_str = session_id_str_owned.as_str();

        // Resolve the session from core.
        let session = self
            .core
            .session_manager
            .get(loom_core::manifest_writer::SessionId(
                session_id_str.to_string(),
            ))
            .map_err(|_| LoomErrorCode::SessionNotFound)?;

        // Reject terminal sessions before any host dispatch.
        // The check must precede host.dispatch() so the shim is never reached.
        {
            use loom_core::budget_enforcer::KillReason;
            use loom_core::session_manager::SessionStatus;
            let status = *session.status.lock();
            match status {
                SessionStatus::Closed => return Err(LoomErrorCode::SessionClosed),
                SessionStatus::Aborted | SessionStatus::Killed | SessionStatus::Crashed => {
                    // Distinguish budget-driven kills from user/store aborts so
                    // the typed `budget-exceeded` code reaches the wire — the
                    // `kill_reason` was written by the budget kill callback
                    // before status flipped, so we just have to consult it.
                    return Err(match session.kill_reason.lock().as_ref() {
                        Some(KillReason::BudgetExceeded { .. }) => LoomErrorCode::BudgetExceeded,
                        _ => LoomErrorCode::SessionAborted,
                    });
                }
                SessionStatus::Created | SessionStatus::Active => {}
            }
        }

        // Per-session dispatch fence: serialize surface-verb dispatch per
        // session and fail fast (`too_many_requests`) while a previous —
        // possibly timeout/cancel-abandoned — dispatch is still running.
        // Held for the WHOLE blocking dispatch; released on every return
        // path via the guard's Drop. Placed AFTER the terminal-status
        // check (cheap rejection first) and BEFORE the activity guard so
        // a fenced-out action never touches `in_flight`.
        let _slot = acquire_dispatch_slot(&session)?;

        // Activity tracking for the idle reaper: mark the action in-flight for the WHOLE
        // dispatch (so an actively-working session is never seen as idle, even a single long
        // navigate) and bump `last_activity` on both entry and exit. The guard's Drop runs on
        // every return path — early returns, errors, panics-as-unwind — so `in_flight` can't
        // leak. Placed AFTER the terminal-status check so closed/aborted sessions don't count.
        session.action_started(now_epoch_ms());
        let _activity = ActionActivityGuard(Arc::clone(&session));

        // Safe profile blocks destructive evaluate.
        // Daemon-layer gate (NOT shim) — daemon has typed Action +
        // Session.profile in scope so we can short-circuit before
        // host.dispatch.
        //
        // NOTE: when adding new destructive verbs (e.g. `web.execute_storage_set`),
        // extend this match. The catchall is intentional — non-evaluate
        // actions pass through unchanged, but a future verb that should
        // be safe-gated will silently bypass this block until added here.
        match &action {
            Action::WebEvaluate { expression, .. } if session.profile == "safe" => {
                // `find_denylist_match` = raw substring pass + a
                // comment/whitespace-stripped pass (catches
                // `document . cookie` / `eval/**/(…)` smuggling). It is
                // a guardrail, not a sandbox — see the threat-model note
                // on `loom_shared::safety::EVALUATE_DENYLIST`.
                if let Some(matched) = loom_shared::safety::find_denylist_match(expression) {
                    tracing::warn!(
                        session_id = %session_id_str,
                        profile = "safe",
                        matched_pattern = matched,
                        "blocked destructive evaluate"
                    );
                    return Ok(profile_restricted_evaluate_receipt(
                        session.allocate_action_id(),
                        session_id_str,
                        matched,
                    ));
                }
            }
            _ => {}
        }

        // v0.9.7 follow-up A — Grant resolution + Follow-up B — per-cookie
        // validation, both performed daemon-side: under the settled
        // daemon-owns-verbs architecture the WASM guest forwards opaquely and
        // the daemon owns the safety/cookie checks (cookie types live in
        // `loom_shared::cookie_types`). Together they
        // bring the cookie verbs up to the user-facing contract
        // documented in the v0.9.6 task spec:
        //
        //   - `set_cookies` with `CookieSource::Grant` resolves through
        //     `core.vault.substitute_cookies(grant_id, session_id)` and
        //     dispatches with the resolved cookie array as if the
        //     operator had supplied `CookieSource::Inline { cookies }`.
        //   - Per-cookie validation (`validate_cookie_params`: 64-cap,
        //     name/value/expires) runs BEFORE the CDP envelope is
        //     built. Validation failures short-circuit to a typed
        //     `cookie_validation_error` receipt (matching the
        //     existing WASM-side ErrorMapper wire kind and the
        //     action_registry --help docstring) without ever
        //     touching the chromium shim.
        //
        // Shape errors (malformed `source` JSON, missing `cookies` key
        // in the vault blob) flow through the existing
        // `SchemaViolation` / `InternalError` codes — only typed
        // `CookieValidationError` variants flow through the
        // taxonomy-coded `cookie_validation_error` receipt. This split
        // keeps the wire taxonomy a closed set the operator can
        // group dashboards on.
        let action = match action {
            Action::WebSetCookies { session_id, source } => {
                use loom_core::manifest_writer::SessionId;
                use loom_core::vault::GrantId;
                use loom_shared::cookie_types::CookieSource;

                // Step 1: parse the `source` payload into the typed
                // `CookieSource` enum so malformed shapes (missing
                // `source` tag, unknown variants, wrong-typed fields)
                // fail closed via `SchemaViolation` rather than
                // silently falling through to the no-op chromium-args
                // branch with an empty cookies array.
                let typed_source: CookieSource =
                    serde_json::from_value(source).map_err(|_| LoomErrorCode::SchemaViolation)?;

                // Step 2: resolve a `Grant` to its cookies array by
                // calling `Vault::substitute_cookies`. The vault blob
                // schema is the canonical
                // `{"schema_version":1,"cookies":[NetworkCookieParam...]}`
                // produced by `loom vault add --credential-type cookie`;
                // a missing or non-array `cookies` field means the
                // keychain blob is corrupt and we fail closed with
                // `InternalError` rather than silently emitting an
                // empty Network.setCookies envelope.
                let cookies: Vec<loom_shared::cookie_types::NetworkCookieParam> = match typed_source
                {
                    CookieSource::Inline { cookies } => cookies,
                    CookieSource::Grant { grant_id } => {
                        let bytes = self
                            .core
                            .vault
                            .substitute_cookies(GrantId(grant_id), SessionId(session_id.clone()))
                            .map_err(|e| map_loom_error(&e))?;

                        #[derive(serde::Deserialize)]
                        struct VaultCookieBlob {
                            cookies: Vec<loom_shared::cookie_types::NetworkCookieParam>,
                        }

                        serde_json::from_slice::<VaultCookieBlob>(&bytes)
                            .map_err(|e| {
                                tracing::error!(
                                    "vault.substitute_cookies blob deserialise failed: {e}"
                                );
                                LoomErrorCode::InternalError
                            })?
                            .cookies
                    }
                };

                // Step 3: per-cookie validation. Failures short-circuit
                // to a typed `cookie_validation` receipt with the
                // snake_case taxonomy code from `cookie_validation_code`.
                if let Err(e) = loom_shared::cookie_types::validate_cookie_params(&cookies) {
                    return Ok(cookie_validation_error_receipt(
                        session.allocate_action_id(),
                        session_id_str,
                        cookie_validation_code(&e),
                        e.to_string(),
                    ));
                }

                // Step 4: rebuild the action with a uniform `inline`
                // source so the downstream `build_chromium_args` path
                // sees a single shape regardless of how the operator
                // framed the request.
                let resolved_source = serde_json::json!({
                    "source": "inline",
                    "cookies": cookies,
                });
                Action::WebSetCookies {
                    session_id,
                    source: resolved_source,
                }
            }
            // web.set_input_files: AUTHORITATIVE upload allow-list gate
            // (plan-council FND#5 — daemon-side, before the wasm guest /
            // chromium ever see the paths). Enforced in ALL profiles, fail
            // closed when LOOM_UPLOAD_ROOT is unset. On success the action is
            // rebuilt with CANONICALIZED paths so chromium opens the validated
            // file (closing most of the canonicalize→read TOCTOU window).
            Action::WebSetInputFiles {
                session_id,
                selector,
                paths,
            } => {
                match upload_guard::validate_upload_paths(
                    &paths,
                    self.upload_root.as_deref(),
                    upload_guard::MAX_UPLOAD_FILES,
                    upload_guard::MAX_UPLOAD_FILE_BYTES,
                    upload_guard::MAX_UPLOAD_TOTAL_BYTES,
                ) {
                    Ok(canon) => Action::WebSetInputFiles {
                        session_id,
                        selector,
                        paths: canon
                            .into_iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                    },
                    Err(e) => {
                        tracing::warn!(
                            session_id = %session_id_str,
                            kind = e.kind(),
                            "blocked file upload (upload allow-list)"
                        );
                        return Ok(upload_error_receipt(
                            session.allocate_action_id(),
                            session_id_str,
                            e.kind(),
                            e.message(),
                        ));
                    }
                }
            }
            other => other,
        };

        let handle = tokio::runtime::Handle::current();

        if let Some(receipt) =
            self.intercept_media_verbs(&action, &session, session_id_str, &handle, deadline_ms)
        {
            return receipt;
        }
        if let Some(receipt) =
            self.intercept_input_verbs(&action, &session, session_id_str, &handle, deadline_ms)
        {
            return receipt;
        }

        let session_handle = SessionHandle {
            session_id: session.id.clone(),
            handle: handle.clone(),
            receipt_pool: handle.clone(),
            abort_flag: session.abort_flag.clone(),
            abort_signal: session.abort_notify.clone(),
            kill_reason: session.kill_reason.clone(),
            seed: session.seed,
            // The session's OWN harness — per-session RNG/clock isolation
            // (never the facade singleton).
            determinism: session.determinism.clone(),
            epoch_ms: session.epoch_ms,
            no_blocklist: session.no_blocklist,
            // settle-capture (4b): thread the determinism toggle through.
            no_determinism: session.no_determinism,
            // voice-call-io: thread the --audio opt-in through to HostState so
            // target-creating host fns carry audio_enabled on the shim wire.
            audio: session.audio,
            // thread profile + downloads_dir into the
            // SessionHandle so HostState can inject env vars at shim spawn.
            profile: session.profile.clone(),
            downloads_dir: session.downloads_dir.clone(),
            // capture-policy=fingerprint tier: derived ONCE here from the
            // authoritative session capture_policy. Gates the post-action DOM
            // fingerprint (host fn + decode accept-gate). Other policies → false
            // → no extra DOM.getDocument round-trip, byte-identical receipts.
            capture_dom_after: session.capture_policy.as_deref() == Some("fingerprint"),
        };

        // Build the host-side action payload. Three shapes flow through
        // here:
        //
        //   1. `web.navigate` — the WIT guest's `navigate-verb` calls
        //      `host::navigate_execute(&url, deadline_ms)` directly; its
        //      `Action.payload` MUST be raw UTF-8 URL bytes (per the
        //      contract documented at `loom-surface-web::navigate_verb`).
        //      `build_chromium_args` produces a CBOR-shaped CdpMessage,
        //      which `String::from_utf8` rejects — so navigate skips
        //      that path.
        //   2. Other web verbs (click/type/etc.) — route through the
        //      generic `host::shim_call("chromium", &a.payload)` path,
        //      where the chromium shim expects a CBOR-encoded CdpMessage.
        //      `build_chromium_args` produces exactly that.
        //   3. Anything else — fall back to JCS-encoded Action.
        let args_canonical_bytes = match &action {
            // Navigate AND evaluate use the typed host functions
            // (`navigate_execute` / `evaluate_execute`) — both expect
            // raw UTF-8 bytes in the action payload, NOT a CBOR-encoded
            // CdpMessage. Mismatch returns `HostError::Internal("payload
            // not valid UTF-8 ...")` from the guest, which surfaces as
            // `internal_error: action dispatch failed` to the CLI.
            // (This regression was caught after the evaluate-result-not-surfaced
            // feature landed.)
            // settle-capture: the navigate payload is `<until>\n<url>` so the
            // readiness mode rides to the guest (which has no JSON parser and
            // splits once on the newline). URLs are http/https/about:blank —
            // never contain a newline — so the split is unambiguous.
            Action::WebNavigate { url, until, .. } => {
                let mode = until.as_deref().unwrap_or("settled");
                format!("{mode}\n{url}").into_bytes()
            }
            Action::WebEvaluate { expression, .. } => expression.as_bytes().to_vec(),
            // web.scroll reuses the evaluate tier: the daemon emits the scroll JS
            // as a RAW expression (not a CBOR CdpMessage), so the guest's
            // `scroll_verb` runs it via `evaluate_execute` and surfaces the
            // post-scroll viewport position. action_hash = sha256(this expression)
            // — covers selector + deltas + the viewport-target logic. Mirrors the
            // WebEvaluate arm above. (Determinism: the JS string is a pure function
            // of the inputs; replay copies recorded receipt bytes, so old scroll
            // recordings still validate against their own bytes.)
            Action::WebScroll {
                selector,
                delta_x,
                delta_y,
                ..
            } => build_scroll_expression(selector, delta_x.unwrap_or(0), delta_y.unwrap_or(0))
                .into_bytes(),
            // settle-capture: web.wait_for's guest verb (`wait_for_verb`) reads
            // the payload as the readiness mode string and calls the typed
            // `host::wait_for_execute`. Raw UTF-8 `until` bytes (default
            // `settled`) — never a CBOR CdpMessage.
            Action::WebWaitFor { until, .. } => {
                until.as_deref().unwrap_or("settled").as_bytes().to_vec()
            }
            // web.set_input_files: the guest's `set_input_files_verb` decodes
            // {selector, paths} from the payload and calls
            // `host::set_input_files_execute`. Paths here are already
            // canonicalized by the upload gate above. action_hash =
            // sha256(payload) covers selector + canonical paths.
            Action::WebSetInputFiles {
                selector, paths, ..
            } => serde_json::to_vec(&serde_json::json!({
                "selector": selector,
                "paths": paths,
            }))
            .unwrap_or_default(),
            _ => build_chromium_args(&action).unwrap_or_else(|| {
                serde_jcs::to_string(&action)
                    .unwrap_or_default()
                    .into_bytes()
            }),
        };
        // per-session monotonic action_id, allocated at
        // dispatch time. The same id is plumbed through HostState →
        // ReceiptBuilder → ActionReceipt (WAL) → Receipt (RPC reply), so the
        // value the CLI sees matches `loom session inspect` entries[].action_id.
        let host_action = HostAction {
            action_id: session.allocate_action_id(),
            surface: action_surface(&action).to_string(),
            method: action_verb(&action).to_string(),
            args_canonical_bytes,
            // Per-action kill deadline (dispatch metadata; excluded from the
            // hash chain — see Action::deadline_ms). The executor races it
            // against the guest call and traps `request_timeout` on expiry.
            deadline_ms,
        };

        let host = Arc::clone(&self.host);
        // Plain block_on: we're on a spawn_blocking thread (see the
        // WasmHostBridge threading contract) — block_in_place would
        // panic here, and isn't needed off the worker pool.
        let outcome = handle
            .block_on(host.dispatch(host_action, session_handle))
            .map_err(|e| {
                // Surface-side dispatch failed at the wasmtime / IPC layer.
                // Don't drop the error message — it's our only signal for
                // diagnosing why navigate / click / evaluate trapped (e.g.
                // shim crash, WIT signature mismatch, OOM in the guest).
                // The ERROR-level log fires regardless of RUST_LOG since the
                // subscriber's default fallback is `warn`. The wire kind
                // stays `SurfaceTrap` so existing CLI / receipt schemas
                // don't churn.
                tracing::error!(
                    surface = %action_surface(&action),
                    method = %action_verb(&action),
                    error = %e,
                    "host.dispatch failed → SurfaceTrap"
                );
                LoomErrorCode::SurfaceTrap
            })?;

        // video-capture: whole-session auto-start. After a successful navigate
        // (a live page now exists) begin recording if the session opted in via
        // `--record-screencast`. Idempotent — the recorder rejects an
        // already-active start, so later navigates are harmless no-ops and the
        // recording spans the whole session until `shutdown_session` finalizes
        // it (→ recordings.jsonl sidecar). Best-effort: a start failure is
        // logged at debug and never affects the navigate receipt.
        if session.record_screencast
            && matches!(&action, Action::WebNavigate { .. })
            && matches!(&outcome, ActionOutcome::Success { .. })
        {
            if let Err(e) =
                handle.block_on(
                    self.host
                        .start_recording(session_id_str, 300_000, 268_435_456, 10),
                )
            {
                tracing::debug!(
                    session_id = %session_id_str,
                    error = %e,
                    "whole-session screencast auto-start skipped (likely already recording)"
                );
            }
        }

        match outcome {
            ActionOutcome::Success { builder, .. } => {
                let mut receipt = build_navigate_wire_receipt(
                    &builder,
                    session_id_str,
                    session.capture_policy.as_deref(),
                );
                // web.scroll surfaces its post-scroll viewport position through the
                // evaluate tier (return_value_json). Promote it into the purpose-named
                // `scroll_result` field (single source of truth for scroll). Follows
                // the capture policy already applied to return_value_json (both
                // stripped together under `minimal`).
                if matches!(action, Action::WebScroll { .. }) {
                    promote_scroll_result(&mut receipt);
                }
                Ok(receipt)
            }
            ActionOutcome::Aborted { .. } => Err(LoomErrorCode::SessionAborted),
            ActionOutcome::Trapped { loom_error, .. } => {
                // Propagate the typed code the host already mapped.
                // `decode_typed_receipt` translates the WIT
                // `host-error` variant (`shim-failure`,
                // `budget-exceeded`, etc.) into the matching
                // `loom_core::LoomErrorCode`; for genuine wasmtime
                // traps `trap_handler::handle_trap` produces
                // SurfaceTrap. Either way, route through
                // `map_loom_error` so the rpc layer sees the right
                // code instead of a hardcoded SurfaceTrap.
                Err(map_loom_error(&loom_error))
            }
        }
    }

    fn has_chromium(&self) -> bool {
        self.has_chromium
    }
}

/// Build a WasmHostBridge. Tries to create a real `WasmHost`; if the
/// surfaces directory is missing or modules haven't been compiled yet,
/// falls back to the stub that returns `SurfaceUnavailable`.
///
/// Returns both the bridge (for the host service adapter) and the
/// underlying `Arc<WasmHost>` (for the CoreBridge so session-close can
/// trigger shim teardown). When a real WasmHost couldn't be built, the
/// second slot is None.
pub(crate) fn build_host_bridge(
    core: Arc<CoreApiFacade>,
    upload_root: Option<PathBuf>,
) -> (Arc<dyn WasmHostBridge>, Option<Arc<loom_host::WasmHost>>) {
    use loom_host::{HostConfig, ShimChromiumConfig, WasmHost};

    // Surface the upload-root posture at startup so operators aren't left
    // guessing why web.set_input_files fails closed (it denies all uploads
    // when LOOM_UPLOAD_ROOT is unset).
    match &upload_root {
        Some(root) => tracing::info!(
            upload_root = %root.display(),
            "web.set_input_files uploads enabled (LOOM_UPLOAD_ROOT)"
        ),
        None => tracing::info!(
            "web.set_input_files uploads DISABLED — set LOOM_UPLOAD_ROOT to enable (fail-closed)"
        ),
    }

    // Resolve surfaces dir the same way `loom postinstall` writes them
    // (~/.config/loom/surfaces/) so AOT-compiled .cwasm modules are found.
    // The CLI's compiled_defaults() hardcodes `home.join(".config").join("loom")`
    // (cli_config/interfaces.rs), so the daemon MUST mirror that path verbatim.
    // dirs::config_dir() returns ~/Library/Application Support on macOS — wrong.
    let surfaces_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("loom")
        .join("surfaces");

    // resolve Chromium across all install channels. Resolution
    // chain: LOOM_CHROMIUM_PATH env override → pinned `~/.config/loom/chromium/...`
    // → PATH search (chromium / chromium-browser / chrome / google-chrome) →
    // macOS `/Applications/...` → typed `BrowserNotFound`. Pinned wins for
    // replay-bit-equality ; a `tracing::warn!` fires below when
    // the resolver picks a non-pinned source so users know they've lost
    // determinism.
    let chromium_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("loom")
        .join("chromium");
    let shim_chromium = match loom_shared::chromium_resolver::resolve_chromium(&chromium_dir) {
        Ok((chromium, source)) => {
            use loom_shared::chromium_resolver::ChromiumSource;
            if source != ChromiumSource::Pinned {
                tracing::warn!(
                    chromium_path = %chromium.display(),
                    source = ?source,
                    "loom: using system-installed Chromium; replay-bit-equality is \
                     not guaranteed across machines. Run 'loom postinstall' for the \
                     pinned build."
                );
            }
            loom_shared::binary_resolver::resolve_loom_sibling("loom-shim-chromium").map(
                |shim_bin| ShimChromiumConfig {
                    shim_binary_path: shim_bin,
                    chromium_path: chromium,
                },
            )
        }
        Err(not_found) => {
            tracing::error!(
                searched = ?not_found.searched_paths,
                "Chromium not found by any resolver path. \
                 Install via 'brew install --cask chromium' (macOS) or your \
                 distro's package manager (Linux), or run 'loom postinstall' \
                 for the pinned build. session.create will return BrowserNotFound \
                 until Chromium is reachable."
            );
            None
        }
    };

    let has_chromium = shim_chromium.is_some();
    let host_config = HostConfig {
        surfaces_dir,
        shim_chromium,
        ..HostConfig::default()
    };

    match WasmHost::new(Arc::clone(&core), host_config) {
        Ok(host) => {
            let host_for_bridge = Arc::clone(&host);
            (
                Arc::new(WasmBridge {
                    host: host_for_bridge,
                    core,
                    has_chromium,
                    upload_root,
                }),
                Some(host),
            )
        }
        Err(_) => {
            tracing::warn!(
                "WasmHost unavailable — run `loom postinstall` to compile surface modules"
            );
            (Arc::new(StubHostBridge), None)
        }
    }
}
