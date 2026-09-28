//! `LOOM_FAKE_CHROMIUM_SCRIPT`: the scripted settle-capture probe and fault switches.

use serde_json::{json, Value};
use std::sync::OnceLock;

/// settle-capture: deterministic per-tick script for the readiness probe,
/// loaded once from `LOOM_FAKE_CHROMIUM_SCRIPT`. Absent env → a single
/// "immediately settled" entry, reproducing the legacy hard-coded probe
/// response (`[true,"https://fake.test/",0]`) so every non-settle test is
/// unaffected.
#[derive(Debug, Clone)]
pub(crate) struct SettleScript {
    /// Per-tick probe responses `(ready_complete, href, dom_mutations)`.
    /// Never empty. The last entry repeats once the cursor runs off the end.
    pub(crate) probe: Vec<(bool, String, u32)>,
    /// Number of never-finishing in-flight requests to pin (the never-settles
    /// network shape). Zero for normal pages.
    pub(crate) perpetual_inflight: usize,
    /// Probe-tick indices at which the loaded PAGE begins a fresh top-level
    /// navigation it initiated itself (window.location / <meta refresh> /
    /// form-POST). Models the client-side-redirect bug: when the cursor
    /// reaches one of these ticks, the fake queues a NEW `Page.loadEventFired`
    /// gated on the NEXT virtual-time budget arm (real headless Chromium holds
    /// the new document's load while the clock is paused) and pins the probe to
    /// `readyState:"loading"` until the executor re-arms the budget. An
    /// executor that never re-attaches (the bug) stays wedged on the blank
    /// in-flight document exactly like it does against real Chromium; one that
    /// re-arms + re-settles reaches the final page. The href reported while
    /// loading is taken from `probe[idx]`.
    pub(crate) renavigate_at: Vec<usize>,
    /// Probe-tick indices at which the click/nav triggers a CROSS-ORIGIN
    /// top-level navigation whose renderer PROCESS SWAPS. Unlike
    /// `renavigate_at` (a same-process client redirect the stale CDP session
    /// keeps observing), a cross-process swap means the virtual-time budget
    /// armed on the OLD session never produces `virtualTimeBudgetExpired` and
    /// the deferred `Page.loadEventFired` never flushes on the stale
    /// connection — both events land on a new renderer the shim is not
    /// watching. Modeled by entering the `awaiting_rearm`/"loading" gate like
    /// `renavigate_at`, but then SUPPRESSING the load-flush + budget-expiry on
    /// the subsequent re-arm (never-recovering: the stale session stays blind).
    /// This is the adversarial case that reproduces the multi-phase dead-wait;
    /// a correct executor still returns a bounded, typed `timeout` inside its
    /// single shared deadline.
    pub(crate) cross_origin_swap_at: Vec<usize>,
    /// When true, the fake WITHHOLDS the CDP response for the trusted-click
    /// commit event (`Input.dispatchMouseEvent` with `type:"mousePressed"`) —
    /// modelling a cross-origin renderer PROCESS SWAP that tears the stale CDP
    /// session down before the mouse-dispatch ack is written, so the ack never
    /// reaches the host. Reproduces the click-DISPATCH dead-wait (distinct from
    /// `cross_origin_swap_at`, which only gates the later settle probe). A
    /// correct host bounds the dispatch by its budget and returns a typed
    /// "dispatched" outcome (the event WAS sent) instead of dead-waiting the
    /// full recv timeout. `mouseMoved`/`mouseReleased` are still acked so only
    /// the commit event's ack is lost.
    pub(crate) swallow_dispatch_ack: bool,
    /// When true, the fake WITHHOLDS the CDP response for EVERY trusted-click
    /// mouse frame from `mouseMoved` onward (`mouseMoved` + `mousePressed` +
    /// `mouseReleased`) — modelling a cross-origin renderer PROCESS SWAP whose
    /// window opens at/before the move, so the stale CDP session goes blind before
    /// even the PRE-COMMIT (`mouseMoved`, index 0) ack is written. This exercises
    /// the surviving `input dispatch ack timeout before commit` arm that
    /// `swallow_dispatch_ack` (commit-frame only) cannot reach: a correct host must
    /// keep dispatching the committing frames and report a typed "dispatched"
    /// outcome, never a transport `shim_timeout`.
    pub(crate) swallow_dispatch_ack_from_move: bool,
    /// When true, withhold ONLY the PRE-COMMIT (`mouseMoved`) ack; the committing
    /// frames still ack. Models a transient move-ack drop on a LIVE session — the
    /// click still commits, so a correct host returns `Ok` (a dropped move ack
    /// alone must not degrade a fully-committed click to `DispatchedAckPending`).
    pub(crate) swallow_dispatch_ack_move_only: bool,
    /// When true, the committing frame (`Input.dispatchMouseEvent
    /// type:"mousePressed"`) responds with a CDP application ERROR envelope rather
    /// than an ack — a GENUINE dispatch failure (renderer rejects the input),
    /// distinct from a swap ack loss. A correct host surfaces it as a hard failure
    /// (`Some(Err)` arm of `dispatch_input_events`), never a performed click.
    pub(crate) dispatch_cdp_error: bool,
    /// Delay (ms) before acking the trusted-click commit event
    /// (`Input.dispatchMouseEvent type:"mousePressed"`), modelling real Chrome's
    /// browser-process ack round-trip (tens of ms) rather than the fake's usual
    /// in-process instant ack. `0` = instant. Lets a test exercise a
    /// delayed-but-ARRIVING ack: it lands within the dispatch budget → `Ok`, NOT
    /// `DispatchedAckPending` — the regression guard for a dispatch budget
    /// collapsed below real ack latency.
    pub(crate) dispatch_ack_delay_ms: u64,
}

impl SettleScript {
    /// Build the `Runtime.evaluate` response for the settle probe at tick
    /// `idx`. `result.value` is the JSON string `[ready, "href", mutations]`
    /// the host's `parse_probe` expects.
    pub(crate) fn probe_response(&self, idx: usize) -> Value {
        let (ready, href, muts) = self
            .probe
            .get(idx)
            .or_else(|| self.probe.last())
            .cloned()
            .unwrap_or_else(|| (true, "https://fake.test/".to_string(), 0));
        let encoded = json!([ready, href, muts]).to_string();
        json!({ "result": { "type": "string", "value": encoded } })
    }
}

pub(crate) fn default_settle_script() -> SettleScript {
    SettleScript {
        probe: vec![(true, "https://fake.test/".to_string(), 0)],
        perpetual_inflight: 0,
        renavigate_at: Vec::new(),
        cross_origin_swap_at: Vec::new(),
        swallow_dispatch_ack: false,
        swallow_dispatch_ack_from_move: false,
        swallow_dispatch_ack_move_only: false,
        dispatch_cdp_error: false,
        dispatch_ack_delay_ms: 0,
    }
}

pub(crate) static SETTLE_SCRIPT: OnceLock<SettleScript> = OnceLock::new();

pub(crate) fn settle_script() -> &'static SettleScript {
    SETTLE_SCRIPT.get_or_init(load_settle_script)
}

pub(crate) fn load_settle_script() -> SettleScript {
    let default = default_settle_script();
    let path = match std::env::var("LOOM_FAKE_CHROMIUM_SCRIPT") {
        Ok(p) => p,
        Err(_) => return default,
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fake-chromium: cannot read settle script {path}: {e}");
            return default;
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fake-chromium: malformed settle script JSON: {e}");
            return default;
        }
    };
    let probe: Vec<(bool, String, u32)> = v
        .get("settle_probe")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let e = e.as_array()?;
                    let ready = e.first()?.as_bool()?;
                    let href = e.get(1)?.as_str()?.to_string();
                    let muts = e.get(2)?.as_u64()? as u32;
                    Some((ready, href, muts))
                })
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or(default.probe);
    let perpetual_inflight = v
        .get("perpetual_inflight")
        .and_then(|n| n.as_u64())
        .unwrap_or(0) as usize;
    let renavigate_at: Vec<usize> = v
        .get("renavigate_at")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_u64().map(|n| n as usize))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cross_origin_swap_at: Vec<usize> = v
        .get("cross_origin_swap_at")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_u64().map(|n| n as usize))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let swallow_dispatch_ack = v
        .get("swallow_dispatch_ack")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let swallow_dispatch_ack_from_move = v
        .get("swallow_dispatch_ack_from_move")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let swallow_dispatch_ack_move_only = v
        .get("swallow_dispatch_ack_move_only")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let dispatch_cdp_error = v
        .get("dispatch_cdp_error")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let dispatch_ack_delay_ms = v
        .get("dispatch_ack_delay_ms")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    SettleScript {
        probe,
        perpetual_inflight,
        renavigate_at,
        cross_origin_swap_at,
        swallow_dispatch_ack,
        swallow_dispatch_ack_from_move,
        swallow_dispatch_ack_move_only,
        dispatch_cdp_error,
        dispatch_ack_delay_ms,
    }
}
