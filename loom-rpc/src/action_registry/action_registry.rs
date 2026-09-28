//! Canonical metadata for every JSON-RPC action loom exposes.
//!
//! Single source of truth for `docs/actions.md`, the generated
//! `loom.1` man page, and the registry-driven help paths in
//! `loom action --help` / `loom action <name> --help`.
//!
//! The Rust dispatch enum (`Action`) and the request-router match-arms
//! remain authoritative for execution; this registry is *additive*
//! metadata. The unit test `registry_required_flags_match_router`
//! enforces equality of the required-param sets between this registry
//! and the router so the two cannot silently diverge.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    I64,
    U64,
    /// v0.9.6 web-cookie-injection. JSON object value (validated
    /// against the per-action JSON-Schema daemon-side). The CLI
    /// JSON-parses the raw `--flag '{...}'` value before sending.
    Object,
    /// v0.9.6. JSON array value (e.g. `web.get_cookies` `urls`).
    /// Same coercion + validation flow as `Object`.
    Array,
}

impl fmt::Display for ParamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParamType::String => "string",
            ParamType::I64 => "i64",
            ParamType::U64 => "u64",
            ParamType::Object => "object",
            ParamType::Array => "array",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParamMeta {
    pub name: &'static str,
    pub ty: ParamType,
    pub doc: &'static str,
    pub required: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ActionMeta {
    pub name: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub params: &'static [ParamMeta],
    pub returns: &'static str,
    pub example: &'static [&'static str],
}

impl ActionMeta {
    pub fn surface_prefix(&self) -> &'static str {
        self.name.split('.').next().unwrap_or(self.name)
    }

    pub fn required_param_names(&self) -> impl Iterator<Item = &'static str> {
        self.params.iter().filter(|p| p.required).map(|p| p.name)
    }
}

pub fn find(name: &str) -> Option<&'static ActionMeta> {
    ACTIONS.iter().find(|a| a.name == name)
}

// FUTURE: as more surfaces land (file.*, cloud.*, ...), consider
// grouping into per-surface registries and re-exporting a flat `ACTIONS`
// slice for backward compatibility. For 15 web.* actions today, one flat
// table is the simplest fit — and the `loom action --help` renderer
// already groups output by surface prefix so the UX scales.
//
// FUTURE: ParamType today only models String/I64/U64 because that is
// every type the router actually parses. New variants (e.g. Bool for a
// future `--force` valueless flag, or StringList for repeatable args)
// can be added when a new action introduces them. Adding a variant
// without updating the CLI's `build_action_command` helper is caught
// by the `paramtype_match_is_exhaustive` test in loom-cli.
//
// FUTURE: a `positional: bool` field on ParamMeta would unlock
// natural-CLI invocations like `loom action web.navigate <url>`. Out of
// scope for the initial registry; the existing CLI uniformly takes
// `--key value` so introducing positionals is a separate UX call.

/// Optional per-action kill deadline (ms), shared by every `web.*` verb. The
/// daemon races it against the action and, on expiry, kills it with a typed
/// `request_timeout` receipt WITHOUT fencing the session. Dispatch metadata —
/// threaded out-of-band, so the deadline VALUE never enters the action's hashed
/// identity (`args_canonical_bytes` / the WIT `deadline-ms` field) and replay
/// stays byte-stable (NFR-DET-01). Optional (`required: false`) so the
/// `registry_required_flags_match_router` parity test is unaffected.
const DEADLINE_MS_PARAM: ParamMeta = ParamMeta {
    name: "deadline_ms",
    ty: ParamType::U64,
    doc: "Optional per-action deadline in milliseconds. On expiry the daemon kills the \
          action with a typed `request_timeout` receipt (the session is NOT fenced and the \
          next call succeeds). Omit or 0 for no deadline.",
    required: false,
};

/// Shared `selector` doc for the host-side interaction verbs (web.click,
/// web.type, web.set_input_files) that resolve the locator grammar. Plain CSS
/// stays the default and is byte-identical to before.
const LOCATOR_DOC: &str = "Locator for the target element. Plain CSS (Level 3) by default; \
     or a composable locator joined by ` >> ` segments: `css=<selector>`, `text=<visible text>` \
     (case-insensitive substring, first visible match), `role=<role>[name=\"<accessible name>\"]` \
     (ARIA role + a W3C accessible-name subset; a date/time-family `<input>` is a `textbox`, \
     as in Playwright; the shortest matching accessible name wins), and `frame=<css>` to descend \
     into an iframe. \
     `frame=` is REQUIRED to cross an origin boundary — a bare locator never reaches into a \
     cross-origin frame (e.g. `frame=iframe[src*=\"widget\"] >> css=#send`).";

pub const ACTIONS: &[ActionMeta] = &[
    ActionMeta {
        name: "web.clear_cookies",
        summary: "Clear ALL cookies in the browser's cookie jar (CDP `Network.clearBrowserCookies`).",
        description: "\
Removes every cookie visible to the active session. Useful between \
test phases to guarantee a clean cookie state. No vault interaction; \
this verb empties the live browser jar directly.\n\n\
The audit chain receives a `CookiesCleared{target_id, session_id, count_before}` \
entry BEFORE the CDP call fires (D9 / FND-0050) — `count_before` comes \
from a synchronous `getCookies` peek so the audit captures the pre-clear \
count even if the CDP call later fails. Cookie *names* are not included \
in this audit entry (only the count); use `web.get_cookies` first if \
you need a name-level record before clearing.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `clear_cookies_result: {\"cleared_count\": u32}`.",
        example: &["loom", "action", "web.clear_cookies", "--session", "<SESSION>"],
    },
    ActionMeta {
        name: "web.click",
        summary: "Click an element by CSS selector or a text=/role=/frame= locator.",
        description: "\
Resolves a CSS query selector against the active page and dispatches \
a synthetic click on the matched element. Surfaces selector misses as \
a typed `js_throw` host error rather than a generic 500 — clients can \
distinguish \"no such element\" from \"element raised during click \
handler\" by inspecting the `kind` field of the host error.\n\n\
Animations and transitions are forced to 0s under loom's deterministic \
profile, so click handlers complete synchronously. The receipt's \
`side_effects` records any DOM mutations triggered by the handler.\n\n\
After the click dispatches, loom runs a BOUNDED post-action readiness wait \
(the same settle machine `web.navigate` uses) and records a `settle_outcome` \
on the receipt. The wait is capped strictly inside the RPC deadline, so a \
churny SPA whose page never quiesces returns a bounded \
`settle_outcome: \"timeout\"|\"dom_unstable\"` receipt (the click still \
reported as performed) — never a transport `rpc timeout`. Control the gate \
with `until` (default `settled`): pass `until: \"load\"` on a churny SPA to \
proceed as soon as the load event fires without waiting for full quiescence.",
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
                doc: LOCATOR_DOC,
                required: true,
            },
            ParamMeta {
                name: "until",
                ty: ParamType::String,
                doc: "Post-action readiness state to wait for: `load`, `networkidle`, or `settled` (default). The wait is bounded inside the RPC deadline — a page that never settles yields a `timeout`/`dom_unstable` outcome, not an `rpc timeout`.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`, `side_effects` populated when the click triggered DOM mutations, and `settle_outcome` (`reached`|`timeout`|`dom_unstable`) from the bounded post-action readiness wait. Selector miss → `kind: \"js_throw\"`. The `outcome_hash` is a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint; the `settle_outcome` and settle diagnostics ride observationally and are EXCLUDED from the replay hash chain. Under `--capture-policy fingerprint` the receipt also carries `dom_after_hash`: `sha256` of the normalized post-action DOM — content-bearing and in the manifest hash chain.",
        example: &["loom", "action", "web.click", "--session", "<SESSION>", "--selector", "#submit"],
    },
    ActionMeta {
        name: "web.delete_cookies",
        summary: "Delete a single cookie scoped by (name, url?, domain?, path?) — CDP `Network.deleteCookies`.",
        description: "\
Targeted cookie delete. Matches by `name` plus any combination of \
`url` / `domain` / `path` filters. Use this when you need to invalidate \
a single credential without clearing the whole jar — for example, to \
test sign-out flows in isolation.\n\n\
The verb performs a `getCookies` peek before AND after the CDP call \
to determine `matched: bool` on the receipt — `true` iff a cookie with \
the given `(name, domain, path)` triple was present before and is \
absent after. This makes the verb idempotent under both \"already gone\" \
and \"successful delete\" outcomes.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "name",
                ty: ParamType::String,
                doc: "Cookie name to delete (RFC 6265 token chars).",
                required: true,
            },
            ParamMeta {
                name: "url",
                ty: ParamType::String,
                doc: "Optional URL scoping. If set, CDP derives domain/path from it.",
                required: false,
            },
            ParamMeta {
                name: "domain",
                ty: ParamType::String,
                doc: "Optional domain scoping. Overrides any domain derived from `url`.",
                required: false,
            },
            ParamMeta {
                name: "path",
                ty: ParamType::String,
                doc: "Optional path scoping. Overrides any path derived from `url`.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `delete_cookies_result: {\"name\": String, \"matched\": bool}`.",
        example: &["loom", "action", "web.delete_cookies", "--session", "<SESSION>", "--name", "sid", "--domain", "example.com"],
    },
    ActionMeta {
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
    },
    ActionMeta {
        name: "web.get_cookies",
        summary: "Read cookies from the browser's cookie jar (CDP `Network.getCookies`).",
        description: "\
Returns all cookies visible to the active session, optionally filtered \
by `urls`. No vault interaction — `get_cookies` reads the live browser \
jar directly. The 64-cookie limit and per-cookie validation do not apply \
here (read path).\n\n\
Per D7, raw cookie *values* appear in the operator-facing receipt — \
this verb is intended for grant inspection and replay-fidelity checks. \
Structured logs (host + MCP) scrub values through the redaction \
registry; the receipt JSON returned to the caller is NOT scrubbed.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "urls",
                ty: ParamType::Array,
                doc: "Optional JSON array of URLs to restrict the cookie read. Maps to CDP `Network.getCookies({urls})`. Omit for all cookies in the active jar.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `get_cookies_result: Vec<NetworkCookie>` — full CDP cookie objects (`name, value, domain, path, expires, size, httpOnly, secure, session, sameSite, priority, sourceScheme, sourcePort, partitionKey, partitionKeyOpaque`).",
        example: &["loom", "action", "web.get_cookies", "--session", "<SESSION>"],
    },
    ActionMeta {
        name: "web.hover",
        summary: "Dispatch a mouseover event at a CSS selector.",
        description: "\
Resolves a CSS query selector and dispatches a synthetic `mouseover` \
event at the matched element. Useful for triggering hover-state UI \
(menus, tooltips) before a follow-up `web.click`.\n\n\
Failure mode: selector miss surfaces as `kind: \"js_throw\"`. The \
hover does not wait for the resulting state — pair with `web.wait` \
on a predicate that observes the hover-induced change if you need to \
synchronise.",
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
                doc: "CSS query selector for the element to hover.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. Selector miss → `kind: \"js_throw\"`. The `outcome_hash` is `sha256` of the CDP `Runtime.evaluate` response envelope — a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint. Under `--capture-policy fingerprint` the receipt also carries `dom_after_hash`: `sha256` of the normalized post-action DOM (content-bearing; in the manifest hash chain). Note a hover that only changes CSS/layout — not the DOM tree — yields a `dom_after_hash` equal to the pre-hover DOM.",
        example: &["loom", "action", "web.hover", "--session", "<SESSION>", "--selector", ".menu-toggle"],
    },
    ActionMeta {
        name: "web.inject_audio",
        summary: "Inject caller-provided audio into the page's virtual microphone.",
        description: "\
Feeds caller-provided audio into the page as if it arrived from the \
microphone, so a browser voice agent under test 'hears' a scripted \
utterance. The payload is supplied EITHER inline as base64 (`audio_b64`) \
OR by content-store reference (`blob_ref`) — exactly one — and the daemon \
resolves and size-bounds it before dispatch.\n\n\
The call returns once the audio is ENQUEUED, not when playout completes; \
pass `await_playout: true` (JSON boolean) to wait for the source node to \
end. This registry entry SURFACES the verb (MCP `tools/list`, `action.web.*` \
alias, docs); the injection itself is wired in a later increment and is only \
meaningful on an audio-enabled session.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "blob_ref",
                ty: ParamType::String,
                doc: "Optional content-store hash of the audio payload (mutually exclusive with `audio_b64`).",
                required: false,
            },
            ParamMeta {
                name: "audio_b64",
                ty: ParamType::String,
                doc: "Optional base64-encoded audio payload, inline (mutually exclusive with `blob_ref`); size-bounded before decode.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. `outcome_hash` is a constant enqueue-success marker; playout completion (when `await_playout` is set) is a receipt field and is never hashed.",
        example: &["loom", "action", "web.inject_audio", "--session", "<SESSION>"],
    },
    ActionMeta {
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
    },
    ActionMeta {
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
    },
    ActionMeta {
        name: "web.press_key",
        summary: "Dispatch a real key press (Enter, Tab, …) via CDP Input.dispatchKeyEvent.",
        description: "\
Dispatches a REAL keyboard key event (`isTrusted:true`) through Chrome's \
input pipeline — unlike synthetic events, these pass trust-gating \
frameworks. `key` is a named key (Enter, Tab, Escape, Backspace, Delete, \
ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown, Space) or a single \
printable character. `modifiers` holds any of Control, Alt, Shift, Meta \
(aliases Ctrl/Cmd/Command/Option accepted) for chords like Ctrl+A.\n\n\
With `selector`, the element is focused first; without it the event goes \
to whatever currently has focus. Many forms submit on Enter in a focused \
field.\n\n\
Host-side verb (CDP Input.*), like the trusted `web.click` — does not run \
the WASM guest. Determinism: the action records the logical key + \
modifiers via a fixed US keymap (identical on every OS) and `outcome_hash` \
is a constant dispatch-success marker, so replay stays structural.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "key",
                ty: ParamType::String,
                doc: "Named key (Enter, Tab, Escape, ArrowDown, …) or a single printable character.",
                required: true,
            },
            ParamMeta {
                name: "selector",
                ty: ParamType::String,
                doc: "Optional CSS selector to focus before pressing; omit to target the currently focused element.",
                required: false,
            },
            ParamMeta {
                name: "modifiers",
                ty: ParamType::Array,
                doc: "Optional modifier keys held during the press: Control, Alt, Shift, Meta (Ctrl/Cmd/Command/Option aliases accepted).",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. `outcome_hash` is a per-verb CONSTANT dispatch-success marker (not page-state). Unknown key/modifier → `kind: \"unknown_key\"`; a `selector` matching nothing → `kind: \"selector_not_found\"`.",
        example: &["loom", "action", "web.press_key", "--session", "<SESSION>", "--key", "Enter"],
    },
    ActionMeta {
        name: "web.say",
        summary: "Speak text into the page's microphone via a configured external TTS backend.",
        description: "\
Synthesizes `text` to speech through an operator-configured external TTS \
backend and injects the result into the page's virtual microphone — the \
spoken equivalent of `web.inject_audio`. loom ships NO TTS engine; the \
operator wires one via environment (a command or URL), and the synthesized \
bytes flow through the same injection path and size bounds as any other \
audio payload.\n\n\
The call returns on ENQUEUE; pass `await_playout: true` (JSON boolean) to \
wait for playout to finish. This is a P1, feature-gated verb; this registry \
entry SURFACES it (MCP `tools/list`, `action.web.*` alias, docs) while the \
TTS backend and injection are wired in later increments.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "text",
                ty: ParamType::String,
                doc: "The text to synthesize and speak into the page microphone. Length-bounded by the TTS backend.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. `outcome_hash` is a constant enqueue-success marker. When no TTS backend is configured, a typed error names both backend environment variables.",
        example: &["loom", "action", "web.say", "--session", "<SESSION>", "--text", "hello there"],
    },
    ActionMeta {
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
    },
    ActionMeta {
        name: "web.scroll",
        summary: "Scroll the page (or an element) by a (delta_x, delta_y) offset.",
        description: "\
Scrolls by `(delta_x, delta_y)` CSS pixels. With no `selector` (or with \
`body`/`html`/the document element) it scrolls the viewport via \
`document.scrollingElement` — so \"scroll the page down\" needs no selector. \
With a real CSS selector it scrolls that element. Both deltas are optional \
and default to 0; passing only one is fine. Useful for revealing virtualised \
list rows or triggering scroll-based lazy loading before observing the result.\n\n\
A selector that matches nothing falls back to scrolling the viewport. The \
scroll does not wait for subsequent layout — pair with `web.wait` on a \
predicate that checks the post-scroll state if needed.",
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
                doc: "CSS query selector for the scrollable element. Optional — omit (or use `body`/`html`) to scroll the page viewport.",
                required: false,
            },
            ParamMeta {
                name: "delta_x",
                ty: ParamType::I64,
                doc: "Horizontal scroll offset in CSS pixels. Optional, defaults to 0.",
                required: false,
            },
            ParamMeta {
                name: "delta_y",
                ty: ParamType::I64,
                doc: "Vertical scroll offset in CSS pixels. Optional, defaults to 0.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `scroll_result: {\"x\": <window.scrollX>, \"y\": <window.scrollY>}` — the viewport scroll position after the scroll (clamps at the scroll max).",
        example: &["loom", "action", "web.scroll", "--session", "<SESSION>", "--delta_y", "400"],
    },
    ActionMeta {
        name: "web.select",
        summary: "Set the value of a `<select>` element and dispatch `change`.",
        description: "\
Sets the resolved `<select>` element's `.value` to `value` and \
dispatches a `change` event so any framework-bound listeners (React, \
Vue, etc.) update accordingly. The element must be a `<select>`; a \
non-select target raises `kind: \"js_throw\"`.\n\n\
Loom does not validate that `value` matches one of the `<option>` \
values; the host engine accepts the assignment, and clients should \
either know the valid set or use `web.evaluate` to enumerate options \
first.",
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
                doc: "CSS query selector for the `<select>` element.",
                required: true,
            },
            ParamMeta {
                name: "value",
                ty: ParamType::String,
                doc: "Value to assign. Should match one of the `<option>` `value` attributes.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. Selector miss / non-select target → `kind: \"js_throw\"`. The `outcome_hash` is `sha256` of the CDP `Runtime.evaluate` response envelope — a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint. Under `--capture-policy fingerprint` the receipt also carries `dom_after_hash`: `sha256` of the normalized post-action DOM — content-bearing and in the manifest hash chain (captures the synchronous post-action DOM; use `web.wait_for` first for async effects).",
        example: &["loom", "action", "web.select", "--session", "<SESSION>", "--selector", "#country", "--value", "GB"],
    },
    ActionMeta {
        name: "web.set_cookies",
        summary: "Inject cookies into the browser's network stack via CDP `Network.setCookies`.",
        description: "\
Adds one or more cookies to the active session's cookie store. \
`source` is the typed XOR `CookieSource` JSON: either \
`{\"source\":\"inline\",\"cookies\":[NetworkCookieParam, ...]}` to pass \
cookie material directly, or `{\"source\":\"grant\",\"grant_id\":\"<id>\"}` to \
resolve a session-bound vault grant (see `loom vault add --credential-type \
cookie`). The vault path substitutes raw cookie values inside the daemon — \
values never cross MCP or the WASM guest boundary.\n\n\
Per-cookie validation runs synchronously before the CDP call: 64-cookie \
cap (DoS guard), empty names rejected, RFC 6265 invalid characters in \
names rejected (`= ; , <space> <tab> \"`), values capped at 4096 bytes, \
`expires` constrained to `-1` (session cookie) or `>=1.0` (seconds-since-epoch). \
The set is atomic — any per-cookie validation failure rejects the whole batch \
and short-circuits before CDP dispatch.\n\n\
Receipt records cookie *names* and per-cookie success but never values — \
values are typed `Redacted<String>` and emit `\"[REDACTED]\"` through all \
Debug/Display/Serialize paths. Audit chain receives a `CookiesSubstituted{grant_id, session_id, cookie_names}` \
entry when the grant path resolves (D5 / FND-0050).",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "source",
                ty: ParamType::Object,
                doc: "JSON-encoded `CookieSource`. Inline: `{\"source\":\"inline\",\"cookies\":[{\"name\":\"sid\",\"value\":\"...\",\"domain\":\"...\"}]}`. Grant: `{\"source\":\"grant\",\"grant_id\":\"<id>\"}`.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `set_cookies_result: Vec<SetCookieResult>` — one entry per validated cookie with `success: true`. Typed validation errors (`name_empty` / `name_invalid` / `value_too_large` / `too_many_cookies` / `invalid_expires`) short-circuit pre-CDP and surface as `error_code: \"cookie_validation_error\"` in the receipt details.",
        example: &["loom", "action", "web.set_cookies", "--session", "<SESSION>", "--source", "{\"source\":\"inline\",\"cookies\":[{\"name\":\"sid\",\"value\":\"abc123\",\"domain\":\"example.com\"}]}"],
    },
    ActionMeta {
        name: "web.set_input_files",
        summary: "Upload local files into an <input type=file> by CSS (or a css=/frame= locator).",
        description: "\
Sets one or more local files on a file input element via CDP \
`DOM.setFileInputFiles`, the only reliable way to drive uploads (typing \
into a file input is ignored by browsers and `input.files` is read-only \
to page script). Resolves the selector through the SAME locator grammar as \
web.click/web.type (the `selector` doc below) — so a `css=`-prefixed selector \
works, and `frame=<css> >> css=<inner>` can target a file input inside a \
SAME-PROCESS (incl. same-site cross-origin) iframe. A bare `text=`/`role=` is \
accepted but rarely matches a file input (they are usually visually hidden, so \
have no visible text / accessible name); prefer `css=`/`frame=`. Then sets the \
files and the browser fires native `input`/`change` events so reactive pages \
update.\n\n\
SECURITY: file paths are gated behind the `LOOM_UPLOAD_ROOT` allow-list. \
If `LOOM_UPLOAD_ROOT` is unset the verb fails closed (`kind: \
\"upload_root_not_configured\"`). Paths are canonicalized (symlink-escape \
defense) and must resolve under the root, else `kind: \"upload_path_blocked\"`. \
Enforced in ALL profiles. Per-call caps: 20 files, 100 MiB/file, 200 MiB total \
(`upload_too_many_files` / `upload_file_too_large` / `upload_total_too_large`). Non-existent paths → \
`upload_path_not_found`; selector miss → `selector_not_found`; a non-file \
input target → `not_a_file_input`. Single-file inputs take `paths[0]`.",
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
                doc: LOCATOR_DOC,
                required: true,
            },
            ParamMeta {
                name: "paths",
                ty: ParamType::Array,
                doc: "Absolute file paths to upload. Each must resolve under LOOM_UPLOAD_ROOT. Single-file inputs use paths[0].",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. Security/selector/element errors surface as typed `kind` strings (e.g. `upload_path_blocked`, `selector_not_found`, `not_a_file_input`).",
        example: &["loom", "action", "web.set_input_files", "--session", "<SESSION>", "--selector", "#upload", "--paths", "[\"/fixtures/a.txt\"]"],
    },
    ActionMeta {
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
    },
    ActionMeta {
        name: "web.start_audio_capture",
        summary: "Begin capturing inbound call audio into a bounded in-page buffer.",
        description: "\
Starts recording the INBOUND audio of a WebRTC call on the session's page — \
the remote participant(s), not the injected microphone — into a bounded \
in-page ring buffer. Paired with `web.stop_audio_capture`, which returns the \
buffered audio as a WAV content reference. `max_duration_ms` and `max_bytes` \
cap the capture; on a cap hit the capture truncates (never errors) and the \
stop reports the reason.\n\n\
At most one capture is active per session; the injected microphone track is \
excluded from the capture by provenance so a session never records its own \
injected audio. This registry entry SURFACES the verb; the capture tap is \
wired in a later increment and requires an audio-enabled session.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "max_duration_ms",
                ty: ParamType::U64,
                doc: "Optional capture duration cap in milliseconds; on expiry the capture truncates and stop reports `duration_cap`. Omit or 0 for a safe default.",
                required: false,
            },
            ParamMeta {
                name: "max_bytes",
                ty: ParamType::U64,
                doc: "Optional captured-bytes cap; on hit the capture truncates and stop reports `byte_cap`. Omit or 0 for a safe default.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"` acknowledging capture start. The captured audio is returned by `web.stop_audio_capture`, not here.",
        example: &["loom", "action", "web.start_audio_capture", "--session", "<SESSION>"],
    },
    ActionMeta {
        name: "web.start_recording",
        summary: "Start recording a video (screencast) of the page.",
        description: "\
Starts a CDP `Page.startScreencast` recording on the session's active page \
target. Frames are collected and acked until `web.stop_recording` is called \
(or a cap is hit), then encoded to a `.webm` (VP8/VP9) via a bundled ffmpeg. \
At most one recording is active per session; calling this while a recording \
is already running fails with `kind: \"js_throw\"`.\n\n\
Resource caps (all optional, safe defaults): `max_duration_ms` (default \
300000), `max_bytes` (default 268435456), `frame_rate` (default 10). The \
recording auto-stops when any cap is reached. Frames spill to a temp file so \
a long recording does not hold the whole video in memory.\n\n\
PRIVACY: a recording captures whatever is on screen, INCLUDING any passwords, \
PII, or third-party content rendered during the window — same posture as \
`web.screenshot`. The `.webm` is stored in the local content-addressed store; \
its bytes are excluded from the replay hash chain (only the content hash is \
recorded), so recording never affects replay-equality.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "max_duration_ms",
                ty: ParamType::U64,
                doc: "Optional auto-stop after this many ms (default 300000 = 5 min).",
                required: false,
            },
            ParamMeta {
                name: "max_bytes",
                ty: ParamType::U64,
                doc: "Optional auto-stop once the buffered frames exceed this many bytes (default 268435456 = 256 MiB).",
                required: false,
            },
            ParamMeta {
                name: "frame_rate",
                ty: ParamType::U64,
                doc: "Optional ENCODE/playback frames-per-second for the output .webm (default 10, clamped 1..=60). This is the muxed output rate; the browser still emits a frame per visual change (it does not decimate capture).",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt confirming the recording started (`code: web_action_completed`). The video hash is returned by `web.stop_recording`.",
        example: &["loom", "action", "web.start_recording", "--session", "<SESSION>"],
    },
    ActionMeta {
        name: "web.stop_audio_capture",
        summary: "Stop inbound audio capture; return the buffered audio as a WAV reference.",
        description: "\
Stops the active inbound-audio capture started by `web.start_audio_capture`, \
drains the in-page buffer, resamples to 16 kHz mono, and returns the result \
as a WAV content reference plus a `stop_reason` (explicit, a cap hit, no \
inbound track, or session close). The returned reference is fetchable to a \
playable `.wav` via `loom blob get`.\n\n\
The captured audio is OBSERVATIONAL — it is excluded from the replay hash \
chain, so a voice session must run with determinism disabled. This registry \
entry SURFACES the verb; the drain, resample, and WAV mux are wired in a \
later increment. Calling it without an active capture is a typed error.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `audio_after_hash` (or an audio blob reference; fetch via `loom blob get <hash>`) and `stop_reason` (one of explicit, byte_cap, duration_cap, no_samples, no_inbound_track, session_closed, error).",
        example: &["loom", "action", "web.stop_audio_capture", "--session", "<SESSION>"],
    },
    ActionMeta {
        name: "web.stop_recording",
        summary: "Stop the active video recording and return its content hash.",
        description: "\
Stops the recording started by `web.start_recording`, encodes the collected \
frames to a `.webm`, stores it in the content-addressed store, and returns the \
content hash. Fails with `kind: \"js_throw\"` if no recording is active, or if \
the recording captured zero frames.\n\n\
If the bundled ffmpeg encoder is unavailable (download failed / offline / the \
`video` build feature is off) the action returns an error receipt with an \
actionable message, but the session itself is unaffected — recording is \
best-effort and never aborts the session.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `screencast_after_hash: <sha256>` pointing to the `.webm` in CAS — fetch via `loom blob get <hash>`. A best-effort encode failure (ffmpeg unavailable / zero frames) returns an error receipt (`kind: \"recording_failed\"`) without aborting the session.",
        example: &["loom", "action", "web.stop_recording", "--session", "<SESSION>"],
    },
    ActionMeta {
        name: "web.type",
        summary: "Focus an input (by CSS or a text=/role=/frame= locator) and type text into it.",
        description: "\
Resolves the selector, focuses the element, and enters `text`.\n\n\
By default (`mode: \"fill\"`) loom selects the field's existing content and \
commits `text` via a single CDP `Input.insertText` — a GENUINE \
(`isTrusted:true`) edit through the browser's editing pipeline, the same \
mechanism as Playwright `fill()`. This drives React/Vue/react-hook-form \
`onChange` AND is treated as user-entered, so trust-gating flows (e.g. Auth0 \
New Universal Login) advance. `Input.insertText` over the selection means \
`text: \"\"` clears the field. Fill acts on the element the selector \
RESOLVED to, so a `role=`/`text=`/`css=`/`frame=` locator replaces the \
field's content exactly like a bare CSS selector.\n\n\
Chromium ignores `Input.insertText` on date/time-family inputs, so fill sets \
`<input type=date|time|datetime-local|month|week|color|range>` BY VALUE, the \
way their native pickers do: the (trimmed) text goes through the native \
`HTMLInputElement` value setter, is read back, and `input` then `change` fire \
(synthetic, `isTrusted:false` — Chromium has no trusted edit path for these \
controls). \
The text must be in the input's own value format (`yyyy-mm-dd` for `date`, \
`hh:mm` for `time`, `yyyy-mm-ddThh:mm` for `datetime-local`, `yyyy-mm`, \
`yyyy-Www`, lower-case `#rrggbb`, a number the range allows); a value the \
input rejects or normalises → `kind: \"malformed_value\"` (the browser leaves \
the field empty). `text: \"\"` clears a date/time/month/week input; `color` and \
`range` cannot be empty, so it is `malformed_value` there. In `fill`, a disabled \
(including by a disabled `<fieldset>`) or readonly target → \
`kind: \"not_editable\"`, and nothing is written. An element the page removes or \
replaces before it can be filled → `kind: \"type_failed\"`.\n\n\
`mode: \"value\"` is the legacy path: set `.value` via `Runtime.evaluate` + \
synthetic `input`/`change` events (`isTrusted:false`). It updates the DOM \
value but trust-gating frameworks treat it as not user-entered — kept as a \
back-compat escape hatch (it takes a bare CSS selector, and writes even a \
disabled field or a value the input would normalise). `mode: \"keystrokes\"` \
dispatches a REAL per-character CDP `Input.dispatchKeyEvent` sequence \
(`isTrusted:true`) — not a way to fill date/time-family inputs, whose \
segmented editors take locale-ordered keys.\n\n\
All three change record-time fidelity only; replay stays structural. \
Failure mode: in `fill`/`keystrokes` a selector miss → \
`kind: \"selector_not_found\"`; in `value` a selector miss → \
`kind: \"js_throw\"`.\n\n\
After a `fill`/`keystrokes` dispatch, loom runs a BOUNDED post-action \
readiness wait (the settle machine `web.navigate` uses) and records a \
`settle_outcome` on the receipt, capped inside the RPC deadline so a churny \
SPA never surfaces a transport `rpc timeout`. Control it with `until` \
(default `settled`; `until: \"load\"` proceeds on the load event). \
`mode: \"value\"` (legacy guest path) is unaffected and does not settle.",
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
                doc: LOCATOR_DOC,
                required: true,
            },
            ParamMeta {
                name: "text",
                ty: ParamType::String,
                doc: "Text to type into the element.",
                required: true,
            },
            ParamMeta {
                name: "mode",
                ty: ParamType::String,
                doc: "Dispatch mode: \"fill\" (default — focus + CDP Input.insertText, Playwright fill() semantics: a genuine isTrusted edit that drives React/react-hook-form onChange and clears on empty text; date/time-family inputs are set by value), \"value\" (legacy — .value via Runtime.evaluate + synthetic events, isTrusted:false; the back-compat escape hatch), or \"keystrokes\" (real per-character CDP Input.dispatchKeyEvent, isTrusted:true). An unrecognized mode behaves as \"value\".",
                required: false,
            },
            ParamMeta {
                name: "until",
                ty: ParamType::String,
                doc: "Post-action readiness state to wait for after a `fill`/`keystrokes` dispatch: `load`, `networkidle`, or `settled` (default). Bounded inside the RPC deadline — a never-settling page yields `timeout`/`dom_unstable`, not an `rpc timeout`. Ignored for `mode: \"value\"`.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"` and (for `fill`/`keystrokes`) `settle_outcome` (`reached`|`timeout`|`dom_unstable`) from the bounded post-action readiness wait. A selector miss in `fill`/`keystrokes` → `kind: \"selector_not_found\"`; in `value` → `kind: \"js_throw\"`. In `fill`: a value a date/time-family input rejects → `kind: \"malformed_value\"`; a disabled/readonly target → `kind: \"not_editable\"`; an element the page removed or replaced before it could be filled, or an unacknowledged prepare step → `kind: \"type_failed\"` (fixed message; no page text). The `outcome_hash` is a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint; the `settle_outcome` and settle diagnostics ride observationally and are EXCLUDED from the replay hash chain. Under `--capture-policy fingerprint` the receipt also carries `dom_after_hash`: `sha256` of the normalized post-action DOM — content-bearing and in the manifest hash chain.",
        example: &["loom", "action", "web.type", "--session", "<SESSION>", "--selector", "#email", "--text", "user@example.com"],
    },
    ActionMeta {
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
    },
    ActionMeta {
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
    },
];
