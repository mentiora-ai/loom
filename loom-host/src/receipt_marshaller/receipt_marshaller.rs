// ReceiptMarshaller — assemble `Receipt` post-WASM-return; queue
// `ManifestWriter::append` on a background tokio task.
//
// # Contract semantics
// - **Off the synchronous return.** `WasmHost::dispatch`
//   returns IMMEDIATELY when the WASM export resolves. This module is
//   invoked AFTERWARDS on a background tokio task spawned on the
//   caller-supplied `receipt_pool` handle (the daemon wires the shared
//   runtime handle, never per host-fn).
// - **Per-session append order.** Spawned tasks are detached, and tokio
//   gives no cross-task ordering — so `queue` chains each task behind
//   the previous one queued for the SAME session. WAL order is
//   hash-chained; scheduler-dependent receipt order would break
//   cross-run hash equality (NFR-DET-01).
// - **Receipt overhead p95 ≤ 50 ms.** Bound by manifest
//   write latency only — assembly is in-memory string + integer ops.
// - **Canonical JSON.** Final payload is
//   `serde_jcs::to_string(receipt)` — never `serde_json::to_string`.
// - **One receipt per action.** Trapped/aborted actions do NOT get a
//   separate marshaller entry point: `TrapHandler`/`SessionExecutor`
//   stamp the truthful status on the action's `ReceiptBuilder` and
//   `WasmHost::dispatch` queues it exactly once.

use super::canonical_bytes::{
    assemble_evaluate_canonical_bytes, assemble_navigate_canonical_bytes,
};
use super::cookies_canonical::assemble_cookies_canonical_bytes;
use crate::wit_type_marshaller::Marshaller;
use loom_core::error::LoomError;
use loom_core::manifest_writer::{ManifestWriter, SessionId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::runtime::Handle as TokioHandle;

/// Per-action receipt builder. Populated by `HostFunctionTable` over
/// the action's lifetime; finalized by `ReceiptMarshaller::queue`.
///
/// `action_hash`, `outcome_hash`, `emitted_at_ms` mirror the WIT
/// `record receipt` fields in `wit/loom-surface.wit:15-19`. They are
/// populated by `SessionExecutor::run` after decoding the typed
/// `result<receipt, host-error>` returned by the WASM guest.
///
/// The `navigate_*` fields are populated from the optional WIT receipt fields
/// when the WASM guest returns a navigate-tier-2 receipt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReceiptBuilder {
    pub action_id: u64,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub status: ReceiptStatus,
    pub side_effects_count: u32,
    pub host_call_count: u32,
    pub error_code: Option<String>,
    pub error_details: Option<String>,
    pub action_hash: String,
    pub outcome_hash: String,
    pub emitted_at_ms: u64,
    /// Whether the navigate marshaller keeps the real (volatile)
    /// `response_body_size_bytes` in the canonical (hashed + HAR-read) receipt.
    ///
    /// **Fail-safe default `false` = zero the sizes.** Response sizes vary
    /// run-to-run (CDN/gzip/chunking) and would break the replay-equal manifest
    /// hash chain (NFR-DET-01), so the SAFE default is to exclude them; a builder
    /// that forgets to set this can only LOSE observability, never corrupt the
    /// chain. Real sizes still ride the non-hashed wire surfaces
    /// (`navigate_network_summary_json.total_bytes`, `navigate_network_entries_json`)
    /// regardless of this flag. `method`/`status`/`content_type` are deterministic
    /// and always kept. Set `true` ONLY for a non-deterministic session
    /// (`--no-determinism`), where real sizes may safely enter the canonical
    /// receipt (→ HAR `content.size`). `SessionExecutor::run` sets it from
    /// `no_determinism`; direct-builder tests that assert real sizes set it true.
    pub preserve_response_sizes: bool,
    // ---- Navigate tier-2 fields ----
    // Populated by decode_typed_receipt when the WIT receipt carries them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_status_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_dom_snapshot_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_screenshot_after_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_console_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_network_count: Option<u64>,
    // settle-capture: the two deterministic readiness fields surfaced on the
    // canonical (and therefore wire) receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_settle_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_settle_outcome: Option<String>,
    /// JSON bytes of `Vec<LoomNetworkEvent>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_side_effects_json: Option<Vec<u8>>,
    /// JSON bytes of `Vec<ShimConsoleLine>`.
    /// Empty list today (current shim console-capture stub).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_console_lines_json: Option<Vec<u8>>,
    /// JSON bytes of `loom_core::NetworkSummary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_network_summary_json: Option<Vec<u8>>,
    /// Observational per-request network entries (NOT hash-chained). JSON
    /// bytes of `Vec<LoomNetworkEntry>` (inline) — None when offloaded to the
    /// content store (see `navigate_network_entries_blob_ref`) or dropped on
    /// offload failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_network_entries_json: Option<Vec<u8>>,
    /// ContentRef when the entries JSON ≥ 64KB. None when inline or dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_network_entries_blob_ref: Option<loom_core::content_store::ContentRef>,
    /// The entries list is incomplete (cap hit or offload failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigate_network_entries_truncated: Option<bool>,
    // ---- Evaluate tier fields ----
    // Populated by decode_typed_receipt when the WIT receipt carries them.
    // Truncation discriminator: evaluate_return_value_blob_ref.is_some().
    /// Canonical-JSON of the evaluated value. None when truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluate_return_value_json: Option<String>,
    /// ContentRef when canonical-JSON bytes > 64KB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluate_return_value_blob_ref: Option<loom_core::content_store::ContentRef>,
    // ---- v0.9.6 cookie tier fields ----
    // Populated by decode_typed_receipt when the WIT receipt carries them.
    // Each is a JSON-encoded payload from the verb's
    // ReceiptBuilder::build_cookies_receipt. Sort/redact transforms
    // happen in `assemble_cookies_canonical_bytes` before JCS encoding
    // (D13 tuple-identity sort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_cookies_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get_cookies_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clear_cookies_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_cookies_result: Option<String>,
    // ---- interaction fingerprint tier (capture-policy=fingerprint) ----
    /// sha256-hex of the normalized post-action DOM for a DOM-mutating selector
    /// interaction (click/type/select/hover), captured via the host
    /// `capture_dom_after_hash` fn. Serializes as `dom_after_hash` on the DEFAULT
    /// (interaction) canonical path → IN the manifest hash chain. `skip_serializing_if`
    /// keeps non-fingerprint receipts byte-identical to pre-feature (NFR-DET-01).
    /// Populated ONLY under the fingerprint tier (host-side accept-gate in
    /// `decode_typed_receipt`), so a misbehaving guest cannot leak it elsewhere.
    #[serde(rename = "dom_after_hash", skip_serializing_if = "Option::is_none")]
    pub interaction_dom_after_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    #[default]
    Ok,
    Error,
    Trapped,
}

/// Per-action observed cost (wall-clock, network bytes, …). Fed to
/// `BudgetEnforcer::account` AFTER the dispatch return.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObservedCosts {
    pub walltime_ms: u64,
    pub network_bytes: u64,
    pub dom_nodes: u64,
    pub js_heap_bytes: u64,
}

/// Local accumulator for off-hot-path receipt enrichment. Populated by
/// `HostFunctionTable` host-fn bodies; consumed during `assemble_canonical_bytes`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SideEffectAccumulator {
    pub host_calls: u32,
    pub blob_puts: u32,
    pub blob_gets: u32,
    pub net_requests: u32,
    pub shim_calls: u32,
}

/// What the marshaller queues. Owned struct — moved into the background
/// task so the dispatch task drops nothing on the hot path.
pub struct ActionOutcome {
    pub session_id: SessionId,
    pub builder: ReceiptBuilder,
    pub observed_costs: ObservedCosts,
}

pub struct ReceiptMarshaller {
    pub(crate) manifest_writer: Arc<dyn ManifestWriter>,
    pub(crate) budget: Arc<dyn loom_core::budget_enforcer::BudgetEnforcer>,
    /// Per-session ordering chain: maps a session to the completion signal
    /// of the LAST append task queued for it. Each new task awaits its
    /// predecessor before appending, so receipts land in the WAL in queue
    /// order even though the tasks themselves are detached and unordered.
    /// One stale `Receiver` per session remains after its last append
    /// (negligible; dropped with the marshaller).
    pub(crate) append_tails: dashmap::DashMap<SessionId, tokio::sync::oneshot::Receiver<()>>,
}

impl ReceiptMarshaller {
    pub fn new(
        manifest_writer: Arc<dyn ManifestWriter>,
        budget: Arc<dyn loom_core::budget_enforcer::BudgetEnforcer>,
    ) -> Arc<Self> {
        Arc::new(Self {
            manifest_writer,
            budget,
            append_tails: dashmap::DashMap::new(),
        })
    }

    /// Queue an action outcome for receipt assembly + manifest append.
    /// Spawns onto `pool`; does NOT block the calling task. Same-session
    /// appends apply in `queue` order: the spawned task first awaits the
    /// completion of the previous task queued for this session (the
    /// `append_tails` swap below is the atomic point that fixes the order).
    pub fn queue(
        self: &Arc<Self>,
        outcome: ActionOutcome,
        pool: TokioHandle,
    ) -> Result<(), LoomError> {
        let this = self.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let prev_tail = self
            .append_tails
            .insert(outcome.session_id.clone(), done_rx);
        pool.spawn(async move {
            if let Some(prev) = prev_tail {
                // Err = predecessor dropped without signalling (panicked or
                // its runtime shut down); it can no longer append, so
                // proceeding cannot reorder the chain.
                let _ = prev.await;
            }
            let session_id = outcome.session_id.0.clone();
            let action_id = outcome.builder.action_id;
            if let Err(e) = this.append_synchronous_fallback(outcome) {
                tracing::error!(
                    session_id = %session_id,
                    action_id = %action_id,
                    error = %e,
                    "receipt manifest append failed"
                );
            }
            let _ = done_tx.send(());
        });
        Ok(())
    }

    /// Synchronous assemble step. Pure: takes a builder, returns
    /// canonical-JSON bytes ready for `ManifestWriter::append`.
    /// `serde_jcs` is the ONLY canonicalizer.
    ///
    /// When navigate tier-2 fields are present, builds a
    /// `loom_core::ReceiptPayload` to get canonical field names and the
    /// unified serialization path.
    pub fn assemble_canonical_bytes(builder: &ReceiptBuilder) -> Result<Vec<u8>, LoomError> {
        use loom_core::error::LoomErrorCode;

        // A typed navigate error receipt (structured
        // shim-failure detail with a `kind` field) takes the navigate
        // assembly path even when `navigate_url` / `navigate_dom_snapshot_hash`
        // are unset (the WIT error variant doesn't carry the URL — it's
        // bound separately at action-dispatch time). Without this gate
        // extension, error receipts fall through to the generic
        // `serde_jcs::to_string(builder)` path and skip the carefully-
        // crafted `code` / `details` / `message` branching in
        // `assemble_navigate_canonical_bytes` below.
        // Restrict the kind check to the navigate-specific shim-failure
        // kinds so an evaluate js_throw error (kind=js_throw) doesn't get
        // routed through the navigate-friendly-message path. Evaluate
        // errors route via assemble_evaluate_canonical_bytes.
        let detail_kind: Option<String> = builder
            .error_details
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(String::from));

        const NAVIGATE_ERROR_KINDS: &[&str] = &[
            "http_status",
            "dns_failure",
            "connect_refused",
            "tls_error",
            "network_error",
        ];
        const EVALUATE_ERROR_KINDS: &[&str] = &["js_throw", "cbor_unrepresentable"];

        // NOTE: `"shim-failure"` here is the WIT `host-error` variant NAME
        // (wit/loom-surface.wit), staged verbatim into builder.error_code by
        // decode_typed_receipt — NOT `LoomErrorCode::as_wire()` output. It stays
        // kebab-case even after the error-code-consolidation snake_case flip,
        // because the WIT contract owns this string (and it is hashed into the
        // replay receipt — must not change).
        let is_navigate_error = builder.status == ReceiptStatus::Error
            && builder.error_code.as_deref() == Some("shim-failure")
            && detail_kind
                .as_deref()
                .map(|k| NAVIGATE_ERROR_KINDS.contains(&k))
                .unwrap_or(false);

        let is_evaluate_error = builder.status == ReceiptStatus::Error
            && builder.error_code.as_deref() == Some("shim-failure")
            && detail_kind
                .as_deref()
                .map(|k| EVALUATE_ERROR_KINDS.contains(&k))
                .unwrap_or(false);

        // Ensure shim-captured network events make it into
        // ReceiptPayload.network_events even when tier-2 fields aren't
        // wired yet (decouples HAR export from
        // navigate-receipt-tier2-still-missing).
        if builder.navigate_url.is_some()
            || builder.navigate_dom_snapshot_hash.is_some()
            || builder.navigate_side_effects_json.is_some()
            || is_navigate_error
        {
            return assemble_navigate_canonical_bytes(builder);
        }

        if builder.evaluate_return_value_json.is_some()
            || builder.evaluate_return_value_blob_ref.is_some()
            || is_evaluate_error
        {
            return assemble_evaluate_canonical_bytes(builder);
        }

        // v0.9.6 cookie tier — any cookie-result field set routes to the
        // cookies canonical-bytes path with D13 tuple-identity sort and
        // value redaction (for replay byte-identity).
        if builder.set_cookies_result.is_some()
            || builder.get_cookies_result.is_some()
            || builder.clear_cookies_result.is_some()
            || builder.delete_cookies_result.is_some()
        {
            return assemble_cookies_canonical_bytes(builder);
        }

        let json = serde_jcs::to_string(builder)
            .map_err(|e| LoomError::new(LoomErrorCode::Internal, e.to_string()))?;
        Ok(json.into_bytes())
    }

    /// Force-synchronous fallback. Called when the background pool
    /// refuses spawn. Logs a tracing warn before falling through.
    pub fn append_synchronous_fallback(&self, outcome: ActionOutcome) -> Result<(), LoomError> {
        use loom_core::manifest_writer::ManifestEntry;
        let bytes = Self::assemble_canonical_bytes(&outcome.builder)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.manifest_writer.append(
            outcome.session_id,
            ManifestEntry::ActionReceipt {
                action_id: outcome.builder.action_id,
                emitted_at_ms: now_ms,
                receipt_canonical_bytes: bytes,
                prev_hash: String::new(),
            },
        )
    }

    /// Test seam: depend-on `WitTypeMarshaller` is structural — the
    /// marshaller is what `assemble_canonical_bytes` uses for any WIT
    /// types embedded in the receipt payload.
    pub fn _marshaller_dep() -> Result<Marshaller, LoomError> {
        Marshaller::generated_or_panic()
    }
}

#[cfg(test)]
mod queue_order_tests {
    use super::*;
    use loom_core::benchmarks::harness::MockBudgetEnforcer;
    use loom_core::manifest_writer::{AuditKind, ManifestEntry, WriterHandle};
    use parking_lot::Mutex;

    /// Records the action_id of every appended ActionReceipt, in arrival
    /// order. The session's FIRST receipt sleeps before recording — without
    /// the per-session ordering chain in `queue`, the later (fast) tasks
    /// would land first and the order assertion below goes red.
    #[derive(Default)]
    struct RecordingWriter {
        appended: Mutex<Vec<u64>>,
    }

    impl ManifestWriter for RecordingWriter {
        fn open_manifest_with_started_at(
            &self,
            _session: SessionId,
            _budgets: Option<loom_core::budget_enforcer::BudgetLimits>,
            _started_at_ms_override: Option<u64>,
            _capture_policy: Option<String>,
            _seed: Option<u64>,
            _determinism_enabled: bool,
        ) -> Result<WriterHandle, LoomError> {
            // WriterHandle paths are pub(crate) to loom-core — unbuildable
            // here, and this test never opens a manifest.
            Err(LoomError::internal("not used in queue_order_tests"))
        }

        fn append(&self, _session: SessionId, entry: ManifestEntry) -> Result<(), LoomError> {
            if let ManifestEntry::ActionReceipt { action_id, .. } = entry {
                if action_id == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                self.appended.lock().push(action_id);
            }
            Ok(())
        }

        fn append_audit(
            &self,
            _session: SessionId,
            _kind: AuditKind,
            _canonical_bytes: Vec<u8>,
        ) -> Result<(), LoomError> {
            Ok(())
        }

        fn validate(&self, _session: SessionId) -> Result<(), LoomError> {
            Ok(())
        }

        fn checkpoint(&self, _session: SessionId) -> Result<(), LoomError> {
            Ok(())
        }
    }

    // REGRESSION (NFR-DET-01): queue() used to fire-and-forget each append
    // onto the runtime with no ordering, so same-session receipts could land
    // in the WAL in scheduler order instead of dispatch order. Queue many
    // outcomes for one session (the first one slow) and assert the appends
    // applied in exactly queue order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_same_session_appends_apply_in_queue_order() {
        let writer = Arc::new(RecordingWriter::default());
        let marshaller = ReceiptMarshaller::new(writer.clone(), Arc::new(MockBudgetEnforcer));
        let sid = SessionId("01HZQUEUE0ORDER0000000000F".into());

        const N: u64 = 24;
        for id in 1..=N {
            marshaller
                .queue(
                    ActionOutcome {
                        session_id: sid.clone(),
                        builder: ReceiptBuilder {
                            action_id: id,
                            ..Default::default()
                        },
                        observed_costs: ObservedCosts::default(),
                    },
                    tokio::runtime::Handle::current(),
                )
                .unwrap();
        }

        // The append tasks are detached — poll the recorder until the chain
        // drains (bounded; the chain is strictly sequential so N appends
        // complete well inside the deadline).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (writer.appended.lock().len() as u64) < N {
            assert!(
                std::time::Instant::now() < deadline,
                "queued appends did not drain"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let order = writer.appended.lock().clone();
        assert_eq!(
            order,
            (1..=N).collect::<Vec<u64>>(),
            "same-session receipt appends must apply in queue order"
        );
    }
}

// === interaction `dom_after_hash` (capture-policy=fingerprint) — canonical-bytes
// contract (the determinism-critical core; written RED before the field exists). ===
#[cfg(test)]
mod interaction_dom_after_hash_tests {
    use super::*;

    /// A bare interaction-tier builder (no navigate/evaluate/cookie fields) —
    /// routes through the DEFAULT `serde_jcs::to_string(builder)` canonical path.
    fn interaction_builder() -> ReceiptBuilder {
        ReceiptBuilder {
            action_id: 7,
            started_at_ms: 100,
            finished_at_ms: 110,
            status: ReceiptStatus::Ok,
            action_hash: "ah".to_string(),
            outcome_hash: "oh".to_string(),
            emitted_at_ms: 110,
            ..Default::default()
        }
    }

    /// Negative determinism (NFR-DET-01): a non-fingerprint interaction receipt
    /// (`interaction_dom_after_hash = None`) must NOT carry `dom_after_hash` in its
    /// canonical bytes — the field is `skip_serializing_if = "Option::is_none"`, so
    /// default/minimal/full sessions serialize byte-identically to the pre-feature
    /// shape and the manifest hash chain is unperturbed.
    #[test]
    fn none_interaction_dom_after_hash_absent_from_canonical_bytes() {
        let b = interaction_builder();
        assert!(b.interaction_dom_after_hash.is_none());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            !s.contains("dom_after_hash"),
            "default-tier interaction receipt must NOT carry dom_after_hash"
        );
    }

    /// Fingerprint tier: `Some(hash)` lands in the DEFAULT canonical path as
    /// `"dom_after_hash"` (→ in the manifest hash chain), and does NOT route the
    /// receipt through the navigate assembly path (no `url`/navigate-only fields).
    #[test]
    fn some_interaction_dom_after_hash_serializes_in_default_path() {
        let mut b = interaction_builder();
        let h = "a".repeat(64);
        b.interaction_dom_after_hash = Some(h.clone());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            s.contains(&format!("\"dom_after_hash\":\"{h}\"")),
            "fingerprint interaction receipt must carry dom_after_hash in canonical bytes; got: {s}"
        );
        assert!(
            !s.contains("\"url\""),
            "interaction receipt must stay on the default path (no navigate url field)"
        );
    }

    /// NFR-DET-01 byte-exact baseline: a non-fingerprint interaction receipt's
    /// canonical bytes match the EXACT pre-feature interaction shape (the new field
    /// is skip-on-None, so it contributes zero bytes). Pinning the full string
    /// catches ANY unintended shape change, not just a leaked `dom_after_hash`.
    #[test]
    fn default_tier_interaction_canonical_bytes_match_pre_feature_shape() {
        let b = interaction_builder();
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        // Exact pre-feature interaction canonical shape (serde_jcs, lexicographic).
        // Note `error_code`/`error_details` serialize as null (no skip) — that is
        // the pre-existing shape. The new `dom_after_hash` is absent (skip-on-None).
        assert_eq!(
            s,
            r#"{"action_hash":"ah","action_id":7,"emitted_at_ms":110,"error_code":null,"error_details":null,"finished_at_ms":110,"host_call_count":0,"outcome_hash":"oh","preserve_response_sizes":false,"side_effects_count":0,"started_at_ms":100,"status":"ok"}"#
        );
    }

    /// Content-bearing: distinct post-action DOM hashes → distinct canonical bytes
    /// (the property the per-verb-constant `outcome_hash` cannot provide).
    #[test]
    fn distinct_dom_after_hashes_yield_distinct_canonical_bytes() {
        let mut a = interaction_builder();
        a.interaction_dom_after_hash = Some("a".repeat(64));
        let mut b = interaction_builder();
        b.interaction_dom_after_hash = Some("b".repeat(64));
        assert_ne!(
            ReceiptMarshaller::assemble_canonical_bytes(&a).unwrap(),
            ReceiptMarshaller::assemble_canonical_bytes(&b).unwrap(),
            "different post-action DOM must produce different canonical receipt bytes"
        );
    }
}
