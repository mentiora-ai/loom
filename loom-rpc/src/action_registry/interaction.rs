//! The element-interaction verbs: click, hover, press_key, scroll, select, set_input_files, type.

use super::action_registry::{
    ActionMeta, ParamMeta, ParamType, DEADLINE_MS_PARAM, GUEST_LOCATOR_DOC, LOCATOR_DOC,
};

pub(super) const WEB_CLICK: ActionMeta = ActionMeta {
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
        returns: "Receipt with `status: \"ok\"`, `side_effects` populated when the click triggered DOM mutations, and `settle_outcome` (`reached`|`timeout`|`dom_unstable`) from the bounded post-action readiness wait. Selector miss → `kind: \"js_throw\"`. The `outcome_hash` is a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint; the `settle_outcome` and settle diagnostics ride observationally and are EXCLUDED from the replay hash chain. `--capture-policy fingerprint` adds no `dom_after_hash` here: web.click is dispatched host-side as trusted input and does not run the guest's post-action DOM fingerprint.",
        example: &["loom", "action", "web.click", "--session", "<SESSION>", "--selector", "#submit"],
};

pub(super) const WEB_HOVER: ActionMeta = ActionMeta {
        name: "web.hover",
        summary: "Dispatch a mouseover event at an element (CSS or a text=/role=/frame= locator).",
        description: "\
Resolves the selector in the page and dispatches a synthetic `mouseover` \
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
                doc: GUEST_LOCATOR_DOC,
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. Selector miss → `kind: \"js_throw\"`. The `outcome_hash` is `sha256` of the CDP `Runtime.evaluate` response envelope — a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint. Under `--capture-policy fingerprint` the receipt also carries `dom_after_hash`: `sha256` of the normalized post-action DOM (content-bearing; in the manifest hash chain). Note a hover that only changes CSS/layout — not the DOM tree — yields a `dom_after_hash` equal to the pre-hover DOM.",
        example: &["loom", "action", "web.hover", "--session", "<SESSION>", "--selector", ".menu-toggle"],
};

pub(super) const WEB_PRESS_KEY: ActionMeta = ActionMeta {
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
                doc: "Optional locator (the same grammar as `selector` on web.click / web.type: CSS, `css=`, `text=`, `role=`, `frame=`) of the element to focus before pressing; omit to target the currently focused element.",
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
};

pub(super) const WEB_SCROLL: ActionMeta = ActionMeta {
        name: "web.scroll",
        summary: "Scroll the page (or an element) by a (delta_x, delta_y) offset.",
        description: "\
Scrolls by `(delta_x, delta_y)` CSS pixels. With no `selector` (or with \
`body`/`html`/the document element) it scrolls the viewport via \
`document.scrollingElement` — so \"scroll the page down\" needs no selector. \
With a selector (CSS or a locator) it scrolls that element. Both deltas are optional \
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
                doc: "Locator for the scrollable element (plain CSS, or the web.click grammar resolved in the page — `frame=` reaches same-origin frames only). Optional — omit (or use `body`/`html`) to scroll the page viewport.",
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
};

pub(super) const WEB_SELECT: ActionMeta = ActionMeta {
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
                doc: GUEST_LOCATOR_DOC,
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
};

pub(super) const WEB_SET_INPUT_FILES: ActionMeta = ActionMeta {
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
};

pub(super) const WEB_TYPE: ActionMeta = ActionMeta {
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
input rejects or normalises → `kind: \"malformed_value\"`, and the field keeps \
the value it held. `text: \"\"` clears a date/time/month/week input; `color` and \
`range` cannot be empty, so it is `malformed_value` there. In `fill`, a disabled \
(including by a disabled `<fieldset>`) or readonly target → \
`kind: \"not_editable\"`, and nothing is written. An element the page removes or \
replaces before it can be filled → `kind: \"type_failed\"`.\n\n\
`mode: \"value\"` is the legacy path: set `.value` via `Runtime.evaluate` + \
synthetic `input`/`change` events (`isTrusted:false`). It updates the DOM \
value but trust-gating frameworks treat it as not user-entered — kept as a \
back-compat escape hatch (it resolves the locator in the page, so `frame=` \
reaches same-origin frames only, and writes even a \
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
        returns: "Receipt with `status: \"ok\"` and (for `fill`/`keystrokes`) `settle_outcome` (`reached`|`timeout`|`dom_unstable`) from the bounded post-action readiness wait. A selector miss in `fill`/`keystrokes` → `kind: \"selector_not_found\"`; in `value` → `kind: \"js_throw\"`. In `fill`: a value a date/time-family input rejects → `kind: \"malformed_value\"`; a disabled/readonly target → `kind: \"not_editable\"`; an element the page removed or replaced before it could be filled, or an unacknowledged prepare step → `kind: \"type_failed\"` (fixed message; no page text). The `outcome_hash` is a per-verb DISPATCH-SUCCESS marker (CONSTANT per verb), NOT a page-state fingerprint; the `settle_outcome` and settle diagnostics ride observationally and are EXCLUDED from the replay hash chain. Under `--capture-policy fingerprint` only `mode: \"value\"` (the guest path) adds `dom_after_hash` (`sha256` of the normalized post-action DOM, content-bearing and in the manifest hash chain); `fill`/`keystrokes` are dispatched host-side as trusted input and carry none.",
        example: &["loom", "action", "web.type", "--session", "<SESSION>", "--selector", "#email", "--text", "user@example.com"],
};
