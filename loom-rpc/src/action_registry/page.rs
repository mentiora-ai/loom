//! The page-level verbs: navigate, the two waits, evaluate, and the snapshot/screenshot/network-log reads.

use super::action_registry::{ActionMeta, ParamMeta, ParamType, DEADLINE_MS_PARAM};

pub(super) const WEB_EVALUATE: ActionMeta = ActionMeta {
        name: "web.evaluate",
        summary: "Run a JavaScript expression in the page and return the value.",
        description: "\
Executes the supplied JavaScript expression via `Runtime.evaluate` in \
the page's JS context and returns the result as canonical JSON. \
Results larger than 64 KB are returned as a content-addressed blob \
reference (`content_ref`) instead of inline.\n\n\
Failure modes: an uncaught exception in the expression surfaces as \
`kind: \"js_throw\"`. Under the `safe` profile (default for `loom \
session create`) loom blocks destructive patterns — writes to \
`window.location`, `document.write`, and similar — before the \
expression reaches the page. The `standard` profile lifts the \
denylist; `full` removes all guards.\n\n\
Determinism: `Math.random()` is sfc32-seeded from the session seed. \
The clock (`Date.now()`, `performance.now()`, `requestAnimationFrame`, \
`setTimeout`) runs on a deterministic virtual timeline pinned to the \
session epoch — it advances (so client-side animations render) but is \
a pure function of the page's work plus the seed, so two sessions \
created with the same seed produce identical results for an identical \
expression. Because virtual time fast-forwards, client-side \
time-based controls (cooldowns, trial/license gates) are not honored \
during capture and must not be relied on as a security boundary.\n\n\
Security: the expression is executed verbatim in the page. Treat it \
as untrusted code if any portion comes from user input — escape \
appropriately, or prefer `web.click` / `web.type` for typed \
interactions.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "expression",
                ty: ParamType::String,
                doc: "JavaScript expression. Returned value is JSON-canonicalised; >64 KB → content blob ref.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `return_value_json` — a JSON-string-encoded value (e.g. `\"\\\"hello\\\"\"` for a string `\"hello\"`, `\"42\"` for the number 42). Decode with one extra `JSON.parse`. Returns ≥64 KB are stored in CAS and `return_value_json` carries a `{\"content_ref\":\"<sha256>\"}` wrapper instead of inline bytes. `kind: \"js_throw\"` on uncaught exception. The evaluate `outcome_hash` is `sha256(\"E:\" ‖ canonical-JSON return value)` (truncated returns use the domain-separated `E:T:` blob-ref hash) — content-bearing on the evaluated value.",
        example: &["loom", "action", "web.evaluate", "--session", "<SESSION>", "--expression", "document.title"],
};

pub(super) const WEB_NAVIGATE: ActionMeta = ActionMeta {
        name: "web.navigate",
        summary: "Load a URL, follow redirects, capture DOM and screenshot.",
        description: "\
Navigates the active page to `url`, follows redirects, and captures \
both the resulting DOM snapshot and a viewport screenshot. The \
receipt records the final URL after redirects, the HTTP status code, \
and whether redirection occurred.\n\n\
URL allowlist: only `http`, `https`, and `about:blank` are accepted. \
Other schemes (`javascript:`, `file:`, `data:`, etc.) are rejected at \
the CLI before any network activity — surfaces as `kind: \
\"url_blocked\"` on the receipt.\n\n\
Typed errors: HTTP error responses surface as `kind: \"http_status\"` \
with the integer status code; DNS resolution failures surface as `kind: \
\"dns_failure\"` with the underlying Chromium error name (e.g. \
`net::ERR_NAME_NOT_RESOLVED`); other low-level network failures (TLS, \
timeout) surface as `kind: \"network_failure\"`. None of these are \
generic 500s.\n\n\
Readiness: the DOM + screenshot are captured once the page reaches the \
`until` state (default `settled`), not at navigation commit. `settled` \
waits for `load`, network-idle, a stable final URL after client-side \
redirects, and a quiescent DOM, so SPA shells and mid-animation frames \
are never captured. The receipt records `until` and `settle_outcome`:\n\
- `reached` — the requested readiness state was satisfied before the \
bound; the capture is gated on a genuinely ready page. A loaded, \
request-quiet, mutation-quiet page settles well inside the default \
timeout.\n\
- `timeout` — the bound (tick ceiling or wall-clock budget) was hit \
while the load/network condition never went quiet (e.g. a persistent \
connection or perpetual polling). The action still SUCCEEDS and the \
capture proceeds; the verdict only describes how the wait ended.\n\
- `dom_unstable` — the bound was hit while the network was quiet and \
the document complete but the DOM kept mutating (perpetual animation / \
re-render). Distinct from `timeout` so consumers can tell the two \
apart. Like `timeout`, the action still succeeds.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "url",
                ty: ParamType::String,
                doc: "Target URL. Must be `http://`, `https://`, or `about:blank` — other schemes are rejected.",
                required: true,
            },
            ParamMeta {
                name: "until",
                ty: ParamType::String,
                doc: "Readiness state to wait for before capture: `load`, `networkidle`, or `settled` (default).",
                required: false,
            },
            ParamMeta {
                name: "timeout_ms",
                ty: ParamType::U64,
                doc: "Maximum time to wait for the readiness state. Optional; defaults to the daemon's settle timeout.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `url` (final URL after redirects), `status_code`, `redirected: bool`, `until`, `settle_outcome` (`reached`|`timeout`|`dom_unstable`). Failure modes: `kind: \"http_status\"|\"dns_failure\"|\"network_failure\"|\"url_blocked\"`. The navigate `outcome_hash` is `sha256(dom_snapshot_hash ‖ screenshot_after_hash)` — content-bearing (unlike the interaction verbs' constant `outcome_hash`); the screenshot hash rides observationally and is excluded from the replay hash chain.",
        example: &["loom", "action", "web.navigate", "--session", "<SESSION>", "--url", "https://example.com"],
};

pub(super) const WEB_NETWORK_LOG: ActionMeta = ActionMeta {
        name: "web.network_log",
        summary: "Read the per-request network entries observed since the last navigate.",
        description: "\
Returns the raw, complete list of network requests the session has made \
since the most recent `web.navigate` — the navigating document plus every \
xhr/fetch and subresource triggered by it and by subsequent in-session \
actions (clicks, evaluate). Each entry carries `url`, `method`, `status`, \
`resource_type`, `from_cache`, `request_id`, and `ts_ms`. Redirect hops \
share `request_id` (one entry per hop).\n\n\
This is OBSERVATIONAL metadata sourced from the Chrome DevTools Protocol — \
never request/response bodies or headers. It is NOT part of the replay hash \
chain, so ordering is best-effort and not guaranteed identical across \
replays. The list is capped (default 1000 entries); when it exceeds ~64KB \
serialised it is offloaded to the content store and surfaced as \
`network_entries_blob_ref`. `network_entries_truncated` flags an incomplete \
list (cap hit or offload failure). Consumers filter same-origin / asset \
noise themselves; loom returns everything.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `network_entries: [{url, method, status, resource_type, from_cache, request_id, ts_ms}]` (or `network_entries_blob_ref: <sha256>` when offloaded; fetch via `loom blob get <hash>`), plus `network_entries_truncated: bool`.",
        example: &["loom", "action", "web.network_log", "--session", "<SESSION>"],
};

pub(super) const WEB_SCREENSHOT: ActionMeta = ActionMeta {
        name: "web.screenshot",
        summary: "Capture a PNG screenshot of the page or a selected element.",
        description: "\
Calls `Page.captureScreenshot`. Without `selector`, captures the full \
viewport. With `selector`, restricts the capture to the element's \
bounding rect (fails with `kind: \"js_throw\"` if the selector misses).\n\n\
The PNG is stored in the content-addressed blob store; the receipt \
carries a `screenshot_ref` (SHA-256) rather than inline bytes. \
Determinism: client-side animations run to completion on a \
deterministic virtual-time clock and the readiness gate captures the \
settled final frame, so two sessions with the same seed reach the \
same final page state. Screenshot bytes are excluded from the replay \
hash chain (only the settled DOM + content hash are chained).",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "selector",
                ty: ParamType::String,
                doc: "Optional CSS selector. When set, screenshot is clipped to the element's bounding rect.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `screenshot_after_hash: <sha256>` pointing to the PNG in CAS — fetch via `loom blob get <hash>`. With `selector` and a miss → `kind: \"js_throw\"`.",
        example: &["loom", "action", "web.screenshot", "--session", "<SESSION>"],
};

pub(super) const WEB_SNAPSHOT: ActionMeta = ActionMeta {
        name: "web.snapshot",
        summary: "Capture a full DOM snapshot of the active page.",
        description: "\
Calls `DOM.getDocument` with `pierce:true` (matching `web.navigate`) and \
serialises the resulting tree into a content-addressed blob. `pierce:true` \
inlines shadow-DOM and iframe `contentDocument` subtrees, so the snapshot \
covers the full composed page rather than just the top document. The receipt \
carries a `content_ref` (SHA-256) plus a top-level hash so callers can detect \
DOM-state changes without comparing full snapshots.\n\n\
Snapshots include the deterministic profile's effects — frozen time, \
seeded randomness, 0-duration animations — so two snapshots from \
sessions with the same seed and action chain are bit-identical at \
this level. Per-frame `frameId`s (one per inlined shadow/iframe document) \
are stripped during normalisation, so they do not perturb the hash.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `dom_snapshot_hash: <sha256>` pointing to the serialised DOM in CAS — fetch via `loom blob get <hash>`.",
        example: &["loom", "action", "web.snapshot", "--session", "<SESSION>"],
};

pub(super) const WEB_WAIT: ActionMeta = ActionMeta {
        name: "web.wait",
        summary: "Wait until a locator resolves (or until timeout).",
        description: "\
Polls the page until the supplied locator matches at least one element, \
or until `timeout_ms` milliseconds elapse. When `timeout_ms` is omitted, \
loom uses the daemon-configured default (typically 30 s).\n\n\
`selector` accepts the SAME locator grammar as `web.click` / `web.type`: a \
bare value (or `css=`) is a CSS selector and matches on presence; `text=` \
matches a visible element by its text, `role=` by ARIA role + accessible \
name, and a `frame=` prefix scopes into a same-process iframe. The wait \
returns as soon as the locator resolves on any poll iteration.\n\n\
Only the final verdict (resolved vs. timed-out) is recorded on the receipt — \
never the poll count or timing — so replay stays hash-equal regardless of \
how long the element took to appear.\n\n\
Typed error: `kind: \"wait_predicate_false\"` if the locator never \
resolves before the timeout. Use this to fail loud rather than \
chaining a brittle `web.click` against an element that is not yet \
present.\n\n\
`web.wait` polls the CURRENT document and arms no virtual-time budget, so \
it does NOT drive a navigation. After an interaction that triggers a \
top-level navigation (form submit, link click), use `web.wait_for` to \
advance and settle the new page first — then `web.wait` for a selector on \
it if you need to gate on a specific element.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "selector",
                ty: ParamType::String,
                doc: "Locator: a bare/`css=` CSS selector, or `text=`/`role=`/`frame=` (same grammar as web.click). Wait succeeds the first poll where it resolves.",
                required: true,
            },
            ParamMeta {
                name: "timeout_ms",
                ty: ParamType::U64,
                doc: "Maximum wait time in milliseconds. Optional; defaults to the daemon's configured wait timeout (~30 s), clamped to a 600 s ceiling.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"` once the locator resolves. Timeout → `kind: \"wait_predicate_false\"`.",
        example: &["loom", "action", "web.wait", "--session", "<SESSION>", "--selector", "#results", "--timeout_ms", "10000"],
};

pub(super) const WEB_WAIT_FOR: ActionMeta = ActionMeta {
        name: "web.wait_for",
        summary: "Wait until the current page reaches a readiness state (settle-capture).",
        description: "\
Waits for the CURRENT page (no navigation) to reach a readiness state, \
then returns a typed receipt carrying the settle verdict. Unlike \
`web.wait` (which polls for a CSS selector), this gates on page-level \
readiness:\n\n\
- `load` — the load event has fired.\n\
- `networkidle` — `load` + no more than a small in-flight trickle held \
quiet for a quiet window (WebSocket/EventSource excluded, so persistent \
connections never hang it).\n\
- `settled` (default) — `networkidle` + `readyState` complete + the \
final URL stable after client-side redirects + the DOM quiescent.\n\n\
The verdict is a pure function of the recorded per-tick observation \
sequence in virtual ticks (NEVER wall-clock), so a recorded session \
replays to the identical outcome. When readiness is never reached \
(persistent connection, perpetual animation) the call returns a typed \
receipt rather than hanging: `settle_outcome` is `timeout` or \
`dom_unstable` instead of `reached`.\n\n\
Use after a `web.navigate` (or an interaction that triggers async \
re-render) to gate a subsequent `web.screenshot` / `web.snapshot` on \
real readiness instead of a magic sleep.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "until",
                ty: ParamType::String,
                doc: "Readiness state to wait for: `load` | `networkidle` | `settled`. Optional; defaults to `settled`.",
                required: false,
            },
            ParamMeta {
                name: "timeout_ms",
                ty: ParamType::U64,
                doc: "Maximum wait time in milliseconds before the bounded fallback returns a typed `timeout`/`dom_unstable` receipt. Optional; defaults to the daemon's navigate budget.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `settle_until` (the requested mode) and `settle_outcome`: `reached` (the requested state was satisfied before the bound), `timeout` (the bound was hit while the load/network condition never went quiet), or `dom_unstable` (the bound was hit while the network was quiet and the document complete but the DOM kept mutating). `timeout`/`dom_unstable` mean readiness was never reached within the bound — the call still returns, it never hangs.",
        example: &["loom", "action", "web.wait_for", "--session", "<SESSION>", "--until", "settled"],
};
