//! The cookie-jar verbs: clear, delete, get and set cookies.

use super::action_registry::{ActionMeta, ParamMeta, ParamType, DEADLINE_MS_PARAM};

pub(super) const WEB_CLEAR_COOKIES: ActionMeta = ActionMeta {
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
    example: &[
        "loom",
        "action",
        "web.clear_cookies",
        "--session",
        "<SESSION>",
    ],
};

pub(super) const WEB_DELETE_COOKIES: ActionMeta = ActionMeta {
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
};

pub(super) const WEB_GET_COOKIES: ActionMeta = ActionMeta {
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
};

pub(super) const WEB_SET_COOKIES: ActionMeta = ActionMeta {
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
};
