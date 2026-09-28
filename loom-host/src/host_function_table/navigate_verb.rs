// `navigate-execute` host function body (the guest's `web.navigate`): the trait
// method in `host_impl.rs` delegates here. Moved verbatim.

use super::host_function_table::HostState;
use crate::host_function_table::chromium_registration::register_chromium_shim_if_absent;
use crate::host_function_table::host_impl::core_ref_to_wit;
use crate::host_function_table::host_impl::loom_to_wit_error;
use crate::host_function_table::host_impl::main_document_event;
use crate::network_offload::{offload_or_inline_network_entries, NetworkEntriesPayload};
use crate::wit_type_marshaller::loom_surface_bindings::loom::surface::{
    host::Host,
    types::{HostError, NavigateResult},
};

impl HostState {
    pub(super) fn navigate_execute_impl(
        &mut self,
        action_id: String,
        url: String,
        until: String,
        budget_ms: u64,
    ) -> impl ::core::future::Future<Output = Result<NavigateResult, HostError>> + Send + '_ {
        use crate::shim_manager::ShimId;

        // Do ALL sync work up front so the returned future holds only owned
        // Send types (same pattern as net_request / shim_call).
        let session_id_str = self.session_id.0.clone();
        let shim_manager = self.shim_manager.clone();
        let content_store = self.core.content_store.clone();
        let determinism = self.determinism.clone();
        let seed = self.seed;
        let epoch_ms = self.epoch_ms;
        // Compute affirmative wire form. The session
        // stores the negative-form `no_blocklist`; the wire carries the
        // affirmative `blocklist_enabled`. Inverting once at this seam
        // makes wire / log dumps directly readable.
        let blocklist_enabled = !self.no_blocklist;
        // settle-capture (4b): per-session determinism toggle.
        let determinism_enabled = !self.no_determinism;
        // voice-call-io: per-session audio opt-in, carried onto the wire so the
        // shim installs the mic bootstrap on the (lazy-spawned) target.
        let audio_enabled = self.audio;
        let manifest_writer = self.core.manifest_writer.clone();
        let session_id_for_audit = self.session_id.clone();
        let shim_session_id = shim_manager.shim_session_id_for(&session_id_str);
        // Same env-var contract as `shim_call`. Captured
        // here for the lazy-register branch below.
        let profile_for_register = self.profile.clone();
        let downloads_dir_for_register = self.downloads_dir.clone();

        // Resolve effective shim ID and lazy-register if needed.
        // ALL sync work (including error cases) is collected into `maybe_id`
        // so the function returns a single `async move` block (RPITIT contract:
        // exactly one anonymous future type per function).
        let maybe_id: Result<ShimId, HostError> = if self.mode
            == crate::wit_type_marshaller::Mode::Replay
        {
            Err(HostError::Internal(
                "navigate_execute not allowed in replay mode".to_owned(),
            ))
        } else {
            // The safe-profile env stays INLINE here (load-bearing
            // downloads_dir invariant) — the helper handles only the common
            // register core shared with evaluate / set_input_files. The shim's
            // CDP bootstrap reads LOOM_SHIM_PROFILE/LOOM_SHIM_DOWNLOADS_DIR and
            // sends Browser.setDownloadBehavior(allowAndName, downloadPath=$DIR)
            // so Chromium confines downloads to the session-scoped dir. The
            // Browser.downloadWillBegin visibility handler is a deferred
            // follow-up; not implemented today.
            register_chromium_shim_if_absent(&shim_manager, &session_id_str, |config| {
                // voice-call-io: signal the `--audio` opt-in to the shim's
                // supervisor (which spawns Chromium before any `audio_enabled`
                // wire request arrives) so it adds the fake-media launch flags.
                // Threaded exactly like LOOM_SHIM_PROFILE below.
                if audio_enabled {
                    config.env.push(("LOOM_SHIM_AUDIO".into(), "1".into()));
                }
                if profile_for_register == "safe" {
                    config.env.push(("LOOM_SHIM_PROFILE".into(), "safe".into()));
                    match &downloads_dir_for_register {
                        Some(d) => {
                            config
                                .env
                                .push(("LOOM_SHIM_DOWNLOADS_DIR".into(), d.display().to_string()));
                        }
                        None => {
                            // INVARIANT VIOLATION: safe profile sessions always
                            // have downloads_dir (LocalSessionManager::create
                            // populates it). If we land here, the session was
                            // created via a path that skipped the dir creation
                            // — Chromium download confinement is silently
                            // disabled.
                            tracing::error!(
                                session_id = %session_id_str,
                                "safe-profile invariant: safe profile session has no downloads_dir; \
                                 Chromium download confinement DISABLED for this shim"
                            );
                        }
                    }
                }
            })
        };

        async move {
            let effective_id = maybe_id?;

            // target_id = 0 → shim's PageNavigate handler lazy-spawns a
            // target via TargetManager::create_new_target, threading the
            // seed/epoch_ms into the determinism JS injection.
            let outcome = shim_manager
                .send_navigate(crate::shim_manager::SendNavigateParams {
                    id: effective_id,
                    action_id,
                    session_id: shim_session_id,
                    target_id: 0,
                    url: url.clone(),
                    budget_ms,
                    seed,
                    epoch_ms,
                    blocklist_enabled,
                    until,
                    determinism_enabled,
                    audio_enabled,
                })
                .await
                .map_err(loom_to_wit_error)?;

            // Every blocked sub-resource becomes a
            // typed audit entry in the manifest hash chain. JCS sorts
            // keys lexicographically; do not rely on declaration order.
            // Adding a new field requires re-verifying canonical output.
            for blocked in &outcome.blocked_events {
                let canonical = serde_jcs::to_vec(&serde_json::json!({
                    "matched_pattern": blocked.matched_pattern,
                    "reason": blocked.reason,
                    "url": blocked.url,
                }))
                .unwrap_or_default();
                if let Err(e) = manifest_writer.append_audit(
                    session_id_for_audit.clone(),
                    loom_core::manifest_writer::AuditKind::BlockedUrl,
                    canonical,
                ) {
                    tracing::warn!(
                        session = %session_id_for_audit.0,
                        url = blocked.url.as_str(),
                        error = %e,
                        "failed to append BlockedUrl audit entry"
                    );
                }
            }

            // Surface typed network failures as a typed HostError, scoped to
            // THIS navigation's MAIN document only. The shim attributes each
            // Document event to a frame/loader and reports the main-document
            // event's index in `main_document_event_index`; an embedded
            // iframe's 4xx document response, a blocklist-failed iframe
            // document, or a stale/cancelled prior load must NOT fail the
            // whole navigate — those events stay in `network_events` for
            // observability only. Order matters:
            //   1. HTTP 4xx/5xx response: shim's CDP handler appends a
            //      `Network.responseReceived`-derived event whose
            //      `status` is the document's HTTP status. Prefer this
            //      over a paired transport-style error_reason because
            //      Chromium emits ERR_HTTP_RESPONSE_CODE_FAILURE alongside
            //      a 4xx/5xx responseReceived for empty-body responses
            //      (e.g. httpbin /status/404 + /status/500). Surfacing
            //      the actual `status_code` is more actionable than the
            //      `network_error: ERR_HTTP_RESPONSE_CODE_FAILURE` form.
            //      (The shim's `find_main_document_index` mirrors this
            //      HTTP-first preference when picking the main event.)
            //   2. Transport-layer failure (DNS / connect-refused / TLS):
            //      shim pushes a `LoomNetworkEvent` with `error_reason`
            //      from `Network.loadingFailed` OR from
            //      `extract_nav_error_text(Page.navigate response)`.
            // The HTTP-first order is safe: DNS / connect-refused / TLS
            // never produce a status>=400 responseReceived (the
            // connection didn't carry an HTTP response), so they fall
            // through to the transport branch as before.
            // P0: the shim has captured the
            // `Network.responseReceived` events for this navigate even when
            // the navigate is about to surface as a typed error. Hoist the
            // structured JSON form once so the typed-error detail can carry
            // `_network_events` through to session_executor → ReceiptBuilder
            // → marshaller → ReceiptPayload, ensuring HAR export covers
            // failed navigates (4xx + transport errors).
            let network_events_value = serde_json::to_value(&outcome.network_events)
                .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
            let main_document_event =
                main_document_event(&outcome.network_events, outcome.main_document_event_index);

            if let Some(ev) = main_document_event.filter(|e| e.status >= 400) {
                let detail = serde_json::json!({
                    "kind": "http_status",
                    "url": url,
                    "status_code": ev.status,
                    // P0: see comment above; carries the captured network
                    // events through to the typed-error receipt for HAR.
                    "_network_events": network_events_value,
                })
                .to_string();
                return Err(HostError::ShimFailure(detail));
            }
            if let Some(ev) = main_document_event.filter(|e| e.error_reason.is_some()) {
                let kind = ev.error_kind.as_deref().unwrap_or("network_error");
                let chromium_error = ev.error_reason.as_deref().unwrap_or("unknown");
                let detail = serde_json::json!({
                    "kind": kind,
                    "url": url, // navigate target
                    "chromium_error": chromium_error,
                    // P0: internal plumbing — stripped by session_executor before
                    // landing in receipt.error_details (leading underscore signals
                    // "not operator-facing").
                    "_network_events": network_events_value.clone(),
                })
                .to_string();
                return Err(HostError::ShimFailure(detail));
            }

            // Store dom_bytes and screenshot_bytes in ContentStore.
            let dom_ref = content_store
                .put(&outcome.dom_bytes)
                .map_err(loom_to_wit_error)?;
            let ss_ref = content_store
                .put(&outcome.screenshot_bytes)
                .map_err(loom_to_wit_error)?;

            // Session-monotonic timestamp: deterministic per-session virtual
            // clock when determinism is on (cross-run byte-equal), else harness.
            let emitted_at_ms = self
                .deterministic_clock_ms()
                .unwrap_or_else(|| determinism.clock_now());

            // Serialize network events as JSON Vec<LoomNetworkEvent>.
            let side_effects_json = serde_json::to_vec(&outcome.network_events).unwrap_or_default();

            // Serialize console_lines verbatim
            // (current stub returns Vec::new() until shim console capture lands).
            let console_lines_json = serde_json::to_vec(&outcome.console_lines).unwrap_or_default();

            // Aggregate NetworkSummary from captured events. Errors counted
            // as HTTP 4xx/5xx OR transport failures (`error_reason.is_some()`).
            let network_summary = loom_core::receipt_builder::receipt_builder::NetworkSummary {
                total_count: outcome.network_events.len() as u64,
                total_bytes: outcome
                    .network_events
                    .iter()
                    .map(|e| e.response_bytes)
                    .sum(),
                error_count: outcome
                    .network_events
                    .iter()
                    .filter(|e| e.status >= 400 || e.error_reason.is_some())
                    .count() as u64,
            };
            let network_summary_json = serde_json::to_vec(&network_summary).unwrap_or_default();

            // Observational network-entries side-channel (NOT hash-chained) — the
            // serialize + ≥64KB offload-or-inline + fail-open degrade logic lives in
            // `crate::network_offload` (shared with `WasmHost::network_log`). This data
            // must NEVER fail a navigate, so a serialize/put error drops the list and
            // forces truncated=true rather than erroring.
            let (network_entries_payload, network_entries_truncated) =
                offload_or_inline_network_entries(
                    &*content_store,
                    &outcome.network_entries,
                    outcome.network_entries_truncated,
                    &session_id_str,
                );
            let (network_entries_json, network_entries_blob_ref) = match network_entries_payload {
                NetworkEntriesPayload::Inline(bytes) => (Some(bytes), None),
                NetworkEntriesPayload::Offloaded(cref) => (None, Some(core_ref_to_wit(cref))),
                NetworkEntriesPayload::Dropped => (None, None),
            };

            Ok(NavigateResult {
                url: outcome.url,
                final_url: outcome.final_url,
                title: outcome.page_title,
                status_code: outcome.status_code as u32,
                dom_snapshot_hash: dom_ref.sha256,
                screenshot_after_hash: ss_ref.sha256,
                console_count: outcome.console_lines.len() as u64,
                network_count: outcome.network_events.len() as u64,
                side_effects_json,
                emitted_at_ms,
                console_lines_json,
                network_summary_json,
                network_entries_json,
                network_entries_blob_ref,
                network_entries_truncated,
                // settle-capture: pass the shim's readiness verdict through.
                // These are NOT folded into outcome_hash (the guest excludes
                // them) — they are virtual-time-derived diagnostics.
                settle_until: outcome.settle_until,
                settle_outcome: outcome.settle_outcome,
                settle_ms: outcome.settle_ms,
                network_count_at_settle: outcome.network_count_at_settle,
            })
        }
    }
}
