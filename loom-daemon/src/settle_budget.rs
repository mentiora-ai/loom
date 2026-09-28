//! The bounded post-action settle and dispatch budgets for the host-side input
//! verbs (`interaction_settle_budget_ms`, `interaction_dispatch_budget_ms`,
//! `settle_after_input_dispatch`). Split out of `wasm_bridge.rs`.

use crate::wire_receipts::*;
use loom_rpc::host_service_adapter::host_service_adapter::Receipt;
use std::sync::Arc;

/// interactive-settle-bounded: headroom subtracted from the effective deadline
/// when clamping the interaction settle budget, so the bounded settle always
/// returns a receipt BEFORE the logical deadline rather than racing it. Sized to
/// cover the shim's post-settle virtual-time RESUME cleanup (`RESUME_CDP_TIMEOUT`
/// = 2s in `loom-shims`, which runs AFTER the settle verdict and can add up to
/// that much wall-clock) plus ~1s margin — so `settle_budget + resume` still
/// lands inside the deadline (click-cross-origin-until ship FND-0001/0009). If
/// `RESUME_CDP_TIMEOUT` changes, revisit this.
const SETTLE_HEADROOM_MS: u64 = 3_000;

// Compile-time guard: the headroom must cover the shim's post-settle resume
// cleanup (`loom-shims` `RESUME_CDP_TIMEOUT` = 2s, which runs AFTER the settle
// verdict) so `settle_budget + resume` still lands inside the deadline (ship
// FND-0001). If RESUME_CDP_TIMEOUT grows past 2s, this breaks the build until
// the headroom is raised to match.
const _: () = assert!(SETTLE_HEADROOM_MS >= 2_000);

/// interactive-settle-bounded: the wall-clock budget for the post-action settle
/// run after a trusted-input dispatch. Mirrors navigate's budget model
/// (`LOOM_SHIM_CDP_TIMEOUT_MS` env → 10s default) so the settle is bounded
/// strictly inside the RPC deadline — a completed click can therefore never
/// surface a transport `rpc timeout`, only a typed `settle_outcome` receipt.
///
/// `deadline_ms` follows loom's session-executor convention: `None` **and
/// `Some(0)`** both mean "no deadline" (`session_executor.rs`) → the base budget
/// (still capped by the server per-call timeout). When the caller passes a REAL
/// (non-zero) deadline, clamp to the budget REMAINING inside the EFFECTIVE
/// deadline (`min(deadline_ms, server_cap)`) after the input dispatch already
/// consumed `elapsed_ms`, minus headroom, so the settle + its cleanup finish
/// before the deadline instead of racing it. There is NO lower floor: a
/// near-exhausted deadline yields a correspondingly small (or zero) budget rather
/// than being floored UP past the deadline — flooring up is exactly the bug that
/// re-tripped `request_timeout` (ship FND-0009). A zero budget returns an
/// immediate typed `timeout` in the shim. (The budget is a record-time wall-clock
/// bound, excluded from the hash chain, exactly like navigate's — NFR-DET-01.)
pub(crate) fn interaction_settle_budget_ms(deadline_ms: Option<u64>, elapsed_ms: u64) -> u64 {
    let base = std::env::var("LOOM_SHIM_CDP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SETTLE_BASE_MS);
    // `server_cap` is loom-rpc's EFFECTIVE per-call timeout (env-aware), read
    // from the single source `connection_handler::request_timeout_ms()` — no
    // duplicated env read / default that could drift (ship FND-0005/0019/0016).
    let server_cap = loom_rpc::connection_handler::request_timeout_ms();
    clamp_settle_budget(base, server_cap, deadline_ms, elapsed_ms)
}

/// interactive-settle-bounded: floor/margin for the trusted-input DISPATCH
/// budget. A real CDP `Input.*` ack is a browser-process round-trip (several to
/// tens of ms), so the dispatch budget MUST sit comfortably above that — a
/// sub-ms budget loses the mouse-ack race on every normal click and reports
/// `click_failed`. This is also the margin the bound leaves under the effective
/// deadline, so a genuinely swapped dispatch's typed return still beats the RPC
/// abandon. A swapped click therefore returns `DispatchedAckPending` after ~1×
/// budget (≥ this margin) — bounded, far under the 30s recv floor.
const DISPATCH_ACK_MARGIN_MS: u64 = 1_000;

/// interactive-settle-bounded: the wall-clock budget for the trusted-input
/// DISPATCH itself (the `Input.*` frames), so a click/type/press whose committing
/// event triggers a cross-origin top-level navigation can never dead-wait the
/// shim's recv floor (~30s) waiting for an ack the swapped-away renderer will
/// never send — the pre-fix bug where the dispatch passed `budget_ms=0`.
///
/// Bounded inside the effective per-call deadline (`min(deadline_ms, server_cap)`;
/// `None`/`Some(0)` ⇒ no caller deadline ⇒ the base budget), capped at the base.
/// **It does NOT reuse [`interaction_settle_budget_ms`]:** the settle formula
/// subtracts a 3s *resume* headroom (for the post-settle virtual-time resume, not
/// the input ack), which would collapse the dispatch budget to ~1ms for any
/// `deadline_ms ≤ 3s` — and a 1ms recv loses the ack race on real Chrome, so a
/// normal same-origin click with a common 1–3s deadline would fail `click_failed`.
/// Instead the dispatch budget is floored to [`DISPATCH_ACK_MARGIN_MS`] (above
/// real ack latency) and leaves only that margin under the deadline. The settle
/// budget IS elapsed-aware, so `dispatch + settle` still fits the deadline. A
/// record-time wall-clock bound, excluded from the hash chain like the settle
/// budget (NFR-DET-01).
pub(crate) fn interaction_dispatch_budget_ms(deadline_ms: Option<u64>) -> u64 {
    let base = std::env::var("LOOM_SHIM_CDP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SETTLE_BASE_MS);
    let server_cap = loom_rpc::connection_handler::request_timeout_ms();
    let effective_deadline = match deadline_ms {
        None | Some(0) => server_cap,
        Some(d) => d.min(server_cap),
    };
    effective_deadline
        .saturating_sub(DISPATCH_ACK_MARGIN_MS)
        .min(base)
        .max(DISPATCH_ACK_MARGIN_MS)
}

/// interactive-settle-bounded default base (`LOOM_SHIM_CDP_TIMEOUT_MS` unset).
const DEFAULT_SETTLE_BASE_MS: u64 = 10_000;

/// Pure core of [`interaction_settle_budget_ms`] — `server_cap` resolved by the
/// caller so this is deterministically unit-testable without process-global env
/// races.
///
/// Clamps the settle budget strictly inside the EFFECTIVE per-call deadline =
/// `min(caller deadline_ms, server_cap)`, minus headroom (which reserves room for
/// the shim's post-settle resume cleanup). The connection handler abandons the
/// RPC at that same effective deadline and returns a transport `request_timeout`,
/// so the budget — AND its floor — must never exceed it: a caller `deadline_ms`
/// larger than the cap (the reported 45s-vs-30s case) is bounded by the cap; a
/// caller `deadline_ms` TIGHTER than the cap bounds the floor too, so a small
/// deadline can't be floored UP past its own budget and re-trip request_timeout
/// (ship FND-0009/0004/0011). All math `saturating_*` (FND-0006) — no underflow.
fn clamp_settle_budget(
    base: u64,
    server_cap: u64,
    deadline_ms: Option<u64>,
    elapsed_ms: u64,
) -> u64 {
    // Effective deadline: the tighter of the caller's deadline and the server
    // cap. `None`/`Some(0)` ⇒ no caller deadline (loom convention) → server cap.
    let effective_deadline = match deadline_ms {
        None | Some(0) => server_cap,
        Some(d) => d.min(server_cap),
    };
    let hard = effective_deadline
        .saturating_sub(elapsed_ms)
        .saturating_sub(SETTLE_HEADROOM_MS);
    // No lower floor: `min(base, hard)` already never exceeds the effective
    // deadline, and a deliberate `hard == 0` (out of time) must stay 0 — flooring
    // it UP to a minimum is exactly the FND-0009 bug (settle overruns the caller's
    // deadline → transport request_timeout). The shim treats a 0 budget as an
    // immediate typed `timeout` (no CDP with a zero deadline).
    base.min(hard)
}

/// interactive-settle-bounded: run the bounded post-action readiness wait after
/// a SUCCESSFUL trusted-input dispatch and fold the verdict onto `receipt`.
/// Reuses the proven, wall-clock-bounded `web.wait_for` settle path
/// (`WasmHost::settle_after_input` → `send_wait_for` → `wait_for_settle`),
/// arming virtual time under the session determinism toggle. Best-effort: the
/// input already dispatched, so a settle transport failure logs and leaves the
/// receipt without a `settle_outcome` rather than failing the verb. The budget
/// is bounded strictly inside the RPC deadline, so the verb can never surface a
/// transport `rpc timeout` — only a typed `settle_outcome`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settle_after_input_dispatch(
    handle: &tokio::runtime::Handle,
    host: &Arc<loom_host::WasmHost>,
    session: &loom_core::session_manager::Session,
    session_id: &str,
    until: Option<&str>,
    deadline_ms: Option<u64>,
    dispatch_elapsed_ms: u64,
    receipt: &mut Receipt,
) {
    let budget = interaction_settle_budget_ms(deadline_ms, dispatch_elapsed_ms);
    let until = until.unwrap_or("settled");
    match handle.block_on(host.settle_after_input(
        session_id,
        until,
        budget,
        session.seed,
        session.epoch_ms,
        !session.no_determinism,
        session.audio,
    )) {
        Ok(outcome) => {
            // Redacted diagnostics: readiness verdict + timing, never page content.
            tracing::debug!(
                session_id = %session_id,
                until = %until,
                settle_outcome = %outcome.settle_outcome,
                settle_ms = outcome.settle_ms,
                network_count = outcome.network_count_at_settle,
                "interactive verb post-action settle complete"
            );
            stamp_settle_outcome(receipt, &outcome);
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "interactive verb post-action settle failed (input still dispatched)"
            );
        }
    }
}

#[cfg(test)]
mod settle_budget_tests {
    use super::*;

    // Pure-function tests (no env): the base + server_cap are passed in, so these
    // are deterministic and race-free (no LOOM_* env mutation, no ENV_LOCK needed).
    const BASE: u64 = DEFAULT_SETTLE_BASE_MS; // 10_000
    const CAP: u64 = 30_000; // illustrative server cap (loom-rpc default)
    const H: u64 = SETTLE_HEADROOM_MS; // 3_000

    #[test]
    fn no_deadline_uses_base_when_under_cap() {
        // None / Some(0) ⇒ no caller deadline ⇒ base (base << cap - headroom).
        assert_eq!(clamp_settle_budget(BASE, CAP, None, 0), BASE);
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(0), 0), BASE);
    }

    #[test]
    fn caller_deadline_clamps_to_remaining_minus_headroom() {
        // deadline 5s, 200ms elapsed ⇒ 5000-200-3000 = 1800, under base.
        assert_eq!(
            clamp_settle_budget(BASE, CAP, Some(5_000), 200),
            5_000 - 200 - H
        );
    }

    #[test]
    fn deadline_larger_than_server_cap_is_clamped_to_cap() {
        // THE REPORTED CASE: caller sends 45s, server cap 30s. The effective
        // deadline is the 30s cap, so budget is base (base < cap-headroom), never
        // 45s — else the RPC race beats the settle → transport request_timeout.
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(45_000), 0), BASE);
        // With a large base (LOOM_SHIM_CDP_TIMEOUT_MS raised past the cap) the
        // server cap - headroom bounds it.
        assert_eq!(clamp_settle_budget(50_000, CAP, Some(45_000), 0), CAP - H);
        assert_eq!(clamp_settle_budget(50_000, CAP, None, 0), CAP - H);
    }

    #[test]
    fn tight_caller_deadline_bounds_budget_not_just_server_cap() {
        // FND-0009: a caller deadline TIGHTER than the server cap must bound the
        // budget (and there is no floor to push it back up). deadline 2s, 300ms
        // elapsed ⇒ 2000-300-3000 saturates to 0 — out of time, budget 0, NOT
        // floored up to a "minimum" that would overrun the 2s caller deadline.
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(2_000), 300), 0);
        // deadline 4s, 0 elapsed ⇒ 4000-3000 = 1000, well under the server cap.
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(4_000), 0), 4_000 - H);
    }

    #[test]
    fn no_floor_up_near_deadline() {
        // FND-0009: near the deadline the budget shrinks to 0 rather than being
        // floored UP (which is what re-tripped request_timeout). No lower bound.
        assert_eq!(clamp_settle_budget(BASE, CAP, None, 27_500), 0); // 30000-27500-3000 sat 0
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(45_000), 29_400), 0);
        // A hair of headroom yields a correspondingly tiny (sub-500) budget, honestly.
        assert_eq!(
            clamp_settle_budget(BASE, CAP, None, 26_800),
            30_000 - 26_800 - H
        ); // 200
    }

    #[test]
    fn saturating_math_no_underflow_past_cap() {
        // elapsed > effective deadline must saturate to 0, never panic (FND-0006).
        assert_eq!(clamp_settle_budget(BASE, CAP, None, 40_000), 0);
        assert_eq!(clamp_settle_budget(BASE, CAP, Some(45_000), 40_000), 0);
        assert_eq!(clamp_settle_budget(BASE, 0, None, 0), 0); // degenerate zero cap
    }

    // (The headroom-covers-resume invariant is enforced at compile time by the
    // `const _: () = assert!(SETTLE_HEADROOM_MS >= 2_000)` guard at the constant's
    // definition — a runtime test would be a clippy `assertions_on_constants`.)
}
