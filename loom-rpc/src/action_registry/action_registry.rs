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

// Each verb's entry is a const in the module for its concern (cookies,
// interaction, page, media); `ACTIONS` below is the one flat list every
// consumer reads. A new surface (file.*, cloud.*, ...) gets its own module
// the same way — the `loom action --help` renderer already groups output
// by surface prefix, so the UX scales.
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
pub(super) const DEADLINE_MS_PARAM: ParamMeta = ParamMeta {
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
pub(super) const LOCATOR_DOC: &str =
    "Locator for the target element. Plain CSS (Level 3) by default; \
     or a composable locator joined by ` >> ` segments: `css=<selector>`, `text=<visible text>` \
     (case-insensitive substring, first visible match), `role=<role>[name=\"<accessible name>\"]` \
     (ARIA role + a W3C accessible-name subset; a date/time-family `<input>` is a `textbox`, \
     as in Playwright; the shortest matching accessible name wins), and `frame=<css>` to descend \
     into an iframe. \
     `frame=` is REQUIRED to cross an origin boundary — a bare locator never reaches into a \
     cross-origin frame (e.g. `frame=iframe[src*=\"widget\"] >> css=#send`).";

/// Shared `selector` doc for the verbs that resolve their target PAGE-side (the
/// guest verbs): the same grammar, but a page cannot reach into a cross-origin
/// frame, so `frame=` covers same-origin frames only.
pub(super) const GUEST_LOCATOR_DOC: &str = "Locator for the target element: plain CSS, or the web.click grammar — \
     `css=` / `text=` / `role=` segments and `frame=<css>`, joined by ` >> `. Resolved in the page, so \
     `frame=` reaches same-origin frames only.";

use super::{cookies, interaction, media, page};

/// Every action, in the order `docs/actions.md` and `loom action --help` list
/// them. Each entry lives with its surface's other verbs.
pub const ACTIONS: &[ActionMeta] = &[
    cookies::WEB_CLEAR_COOKIES,
    interaction::WEB_CLICK,
    cookies::WEB_DELETE_COOKIES,
    page::WEB_EVALUATE,
    cookies::WEB_GET_COOKIES,
    interaction::WEB_HOVER,
    media::WEB_INJECT_AUDIO,
    page::WEB_NAVIGATE,
    page::WEB_NETWORK_LOG,
    interaction::WEB_PRESS_KEY,
    media::WEB_SAY,
    page::WEB_SCREENSHOT,
    interaction::WEB_SCROLL,
    interaction::WEB_SELECT,
    cookies::WEB_SET_COOKIES,
    interaction::WEB_SET_INPUT_FILES,
    page::WEB_SNAPSHOT,
    media::WEB_START_AUDIO_CAPTURE,
    media::WEB_START_RECORDING,
    media::WEB_STOP_AUDIO_CAPTURE,
    media::WEB_STOP_RECORDING,
    interaction::WEB_TYPE,
    page::WEB_WAIT,
    page::WEB_WAIT_FOR,
];
