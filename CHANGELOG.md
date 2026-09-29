# Changelog

All notable changes to loom are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **An invalid selector no longer kills the session's browser** — when Chromium rejected a
  selector it cannot parse (`DOM Error while querying`, e.g. Playwright's `:text()`), the locator
  resolver returned it as an error, `web.click` / `web.type` / `web.press_key` / `web.wait` recorded
  it as a TRANSPORT failure, and that evicted the live shim. The browser was shut down, and every later
  call in the session failed (`surface_trap`). Resolution now follows its documented contract: any
  CDP application error while resolving (an unparseable selector, a node or execution context that
  went away) is a typed `selector_not_found`, and the browser stays up. Only a transport failure
  is an error.

## [0.15.6] — 2026-09-28 — role= names a control by the label wrapping it

A patch release. `role=` locators now name a form control by a `<label>` that wraps it, as
Playwright does, not only by `<label for=…>`: `role=textbox[name="Email address"]` resolves
`<label>Email address <input type="email"></label>`, and a wrapped `<select>` is no longer named
after its options. New recordings of a guest verb (`select`/`hover`/`scroll`/value-mode `type`)
with a `role=` locator get a different `action_hash`; replay is unaffected. (#341)

### Fixed

- **`role=` names a control by the `<label>` wrapping it** — the accessible-name subset read only
  `<label for=…>` and placeholders, so a control nested in its label (`<label><span>Email
  address</span><input type="email"></label>`, no `id`) had no name, and
  `role=textbox[name="Email address"]` did not resolve. A wrapped `<select>` was named after its
  options instead (`role=combobox[name="Alpha"]` matched it). A control's name now comes from its
  labels, `for=` and wrapping alike (Playwright parity), and a wrapping label contributes only its
  own text, never the control's. This applies to both resolver paths: host-side
  `web.click`/`web.type`/`web.press_key`, and the guest verbs. Because the guest verbs hash the
  JS they send, new recordings of a guest verb with a `role=` locator get a different
  `action_hash`; replay is unaffected, and plain CSS is unchanged.

## [0.15.5] — 2026-09-28 — Guest verbs take locators, a bare `web.type` hashes as fill

A patch release. `web.select`, `web.hover`, `web.scroll` and `web.type mode:"value"` now
resolve the same `css=` / `text=` / `role=` / `frame=` locator grammar as `web.click`,
where they used to throw on anything but plain CSS (#324). A bare `web.type`'s receipt
`action_hash` now hashes the `fill` path it takes, not `value` (#322). **Note:** NEW
recordings of a bare `web.type` therefore get a different `action_hash` than older
recordings of the same script; replay is structural and unaffected. Plain-CSS guest-verb
payloads are byte-identical, so those recordings keep their hashes. The rest is internal:
the oversized files are split with no behavior change, and the macOS e2e job's fixture
server is polled instead of slept on. (#338)

### Fixed

- **`web.select`, `web.hover`, `web.scroll` and `web.type mode:"value"` take the locator
  grammar** — these verbs resolve their target inside the page, and ran the selector through
  `document.querySelector`, so a `role=`, `text=`, `css=` or `frame=` locator threw and the verb
  failed. They now resolve the same grammar as `web.click`, page-side (so `frame=` reaches
  same-origin frames only). A plain CSS selector builds exactly the payload it did before, so
  those recordings keep their `action_hash`; a locator-grammar selector, which used to fail, now
  hashes the resolver it runs. (#324)

### Changed

- **`web.type` receipts: `action_hash` keys on the dispatch path** — a bare `web.type` (mode absent) is a
  `fill`, and its receipt `action_hash` now hashes `fill`, the same as an explicit `mode: "fill"`; it was
  labelled `value`, a path it never took. `action_hash` of NEW recordings of a bare `web.type` therefore
  differs from older recordings of the same script (a `loom session diff` across versions shows it);
  replay of existing recordings is unaffected (structural). (#322)
- **Registry docs match the verbs** — `web.click` / `web.type` (fill, keystrokes) no longer claim a
  `dom_after_hash` they do not emit, and `web.press_key` documents the full locator grammar it
  accepts (`docs/actions.md`, `loom action --help`). (#323)
- Internal refactors with no behavior change: split the oversized files — `wasm_bridge.rs` (#316),
  `wire_receipts.rs` (#320), the daemon's `lib.rs` tests (#310), `host_impl.rs` (#315),
  `receipt_marshaller.rs` (#317), the shim manager's `senders.rs` with one CDP-error helper
  (#312, #319, #321), `action_executor.rs` (#314), the `fake-chromium` test binary (#313), the
  action registry (#327), `replay_engine_behavior.rs` (#311) and `integration_shim_e2e.rs` (#326)
  test files, the Python SDK's `loom/__init__.py` (#329), the TypeScript SDK's `session.ts` (#331)
  and `tests/e2e/run_e2e.sh` (#330). Both SDKs export exactly the names they did.
- **e2e harness: the fixture server is polled, not slept on** — the macOS pinned-Chromium e2e
  job had failed at `fixture-server-up` on every run since at least 2026-08-20 (one `sleep 1`,
  then one curl). `run_e2e.sh`, `run_load.sh` and `run_mcp.sh` share one bounded start that logs
  why a start failed. (Internal; no runtime effect.)

## [0.15.4] — 2026-09-28 — Dependency & Security Refresh, date inputs, a fresh mic per call

A patch release. It clears three security advisories and folds in the pending
dependency bumps, makes `web.type` fill native date/time inputs (#332), and gives
every `getUserMedia` its own synthetic microphone track (#335). **RUSTSEC-2026-0268**
(guest-controlled host heap allocation through WASIp3 streams) and **RUSTSEC-2026-0269** (filesystem sandbox
escape when paths or symlinks contain trailing slashes) are fixed by moving wasmtime +
wasmtime-wasi from 47.0.3 to **48.0.3** — a major-version engine bump that required no
code changes and keeps the replay hash chain byte-equal (the full determinism suite
passes unchanged). **RUSTSEC-2026-0285** (TLS 1.3 handshake messages accepted across
encryption-level boundaries) is fixed by moving rustls from 0.23.43 to 0.23.45. The
workspace also moves to wit-bindgen 0.61, jsonschema 0.52, and wasmparser 0.256, plus
the non-major cargo group and an updated CI action. (#306)

### Fixed

- **`web.type` fills native date/time inputs** — Chromium ignores `Input.insertText` on
  `<input type=date|time|datetime-local|month|week|color|range>`, so the default `fill` reported
  success and left the field empty. Fill now sets these inputs by value, the way their native
  pickers do (native `HTMLInputElement` value setter, read-back, then `input` + `change`, so a
  React-controlled input sees the change), and a value the input rejects or normalises is a typed
  `malformed_value` error (the field keeps the value it held) instead of a silent success.
- **`role=` finds date/time inputs** — a date, time, datetime-local, month, week or color
  `<input>` is now a `textbox` to the `role=` locator (Playwright's implicit role), so
  `role=textbox[name="First day"]` resolves a labelled date field. The resolver still keeps the
  shortest matching accessible name, so where a date-family input's label is shorter than today's
  match it now wins a `role=textbox[name=…]` selector. Other input types are unchanged.
- **`web.type` through a locator replaces instead of prepending** — fill's select-all step
  re-queried the raw selector with `document.querySelector`, which throws on `role=`, `text=`,
  `css=` and `frame=` locators, so the field was never cleared and the text landed in front of
  the existing value (`"old"` → `"newold"`). Fill now acts on the element the locator resolved to. (#332)
- **Every `getUserMedia` gets its own synthetic microphone track** — the injected-audio shim
  handed every caller the SAME track, so a page that stopped one call's microphone (the normal way
  to end a WebRTC call) silenced the synthetic mic for the rest of the document, and a second call
  in the same page published an ended track and was never heard. Each `getUserMedia` now returns a
  clone of the injected track; stopping it ends only that clone, and the next call gets a live one.
  (#318, #335)
- **cargo-deny advisories** — RUSTSEC-2026-0268 / RUSTSEC-2026-0269 (Wasmtime) cleared
  via wasmtime + wasmtime-wasi 47.0.3 → 48.0.3; RUSTSEC-2026-0285 (rustls) cleared via
  rustls 0.23.43 → 0.23.45. The yanked chacha20 0.10.1 is replaced by 0.10.2. (#306)

### Changed

- **Dependency refresh** — wasmtime + wasmtime-wasi 48.0.3, wit-bindgen 0.61,
  jsonschema 0.52, wasmparser 0.256, and non-major bumps (clap 4.6.7, toml 1.1.6,
  uuid 1.26.1, flate2 1.1.10, rustix 1.1.5, secret-service 5.2.0). No public API
  changes. (#306)
- **CI actions** — taiki-e/install-action v2.86.5. (#306)
- **`web.type` fill refuses a disabled or readonly target** with `kind: "not_editable"`
  (Playwright parity). A fill into a disabled text field used to succeed silently without
  changing it. `mode: "value"` still writes without checks when that is deliberately wanted.
  (#332)
- **Vendored WASM guest** — `loom-cli/vendor/loom_surface_web.wasm` rebuilt against the
  new dependency set (Linux CI bytes). (#306)

## [0.15.3] — 2026-08-21 — web.click tolerates a navigation racing the trusted-input commit ack

v0.15.2 (#291) bounded the trusted-input **dispatch** and rescued an ack lost on the
**committing** frame (`mousePressed`) of a `web.click` whose cross-origin navigation swaps
the renderer. But a real renderer **process swap** can open at/before the **`mouseMoved`**
frame and swallow the **pre-commit** ack too — which still hit the eager `input dispatch ack
timeout before commit` arm and surfaced a hard `shim_timeout` (a false `click_failed`) even
though the click and its navigation actually happened. The dispatch decision was a pure
frame-index heuristic with no navigation observation. Now a pre-commit ack loss no longer
aborts: the committing frames are still dispatched (so the click is genuinely performed) and
a swap-induced ack loss returns the typed *dispatched* outcome, so the daemon proceeds to the
already-bounded settle and returns a typed `settle_outcome` receipt for the destination — the
click is reported performed, only readiness is degraded when the destination is genuinely
slow. Determinism (NFR-DET-01) is preserved: the dispatched-ack-pending outcome hashes
identically to a normal dispatch, so record-time ack timing never perturbs the manifest hash
chain and replay stays bit-equal. Host-only change. (#293)

### Fixed

- **`web.click` whose handler triggers an immediate top-level (incl. cross-origin)
  navigation no longer surfaces `shim_timeout: input dispatch ack timeout before commit`.**
  The trusted-input dispatch loop no longer treats a **pre-commit** (`mouseMoved`) ack lost
  to a renderer process swap as a hard failure: it keeps dispatching the committing frames
  (so the click is genuinely performed) and reports the swap-induced ack loss as the typed
  "dispatched" outcome, exactly as the committing-frame case already did. A dropped
  `mouseMoved` ack with clean committing acks stays a clean success (`Ok`), so a fully
  committed click is never mislabeled degraded. Genuine dispatch failures — a CDP
  application error, or an unresolvable / unhittable selector — still report failure. The
  per-frame dispatch budget is unchanged for every caller (`web.click` / `web.type` /
  `web.press_key`), so non-click input timeout semantics are untouched. (#293)

### Changed

- The per-frame trusted-input dispatch decision is factored into a pure, exhaustively
  unit-tested classifier (`dispatch_frame_step`), giving the ack-tolerance logic default-CI
  coverage independent of the `#[ignore]`d real-shim e2e. (#293)

## [0.15.2] — 2026-08-21 — web.click dispatch bounded under the per-call deadline

v0.15.1 (#288) bounded the post-action **settle** (the `web.wait_for` path), but the
trusted-input **dispatch** — the `Input.*` frames of `web.click` / `web.type` /
`web.press_key` — still ran with an unbounded CDP recv (it passed `budget_ms = 0`, so
the recv floored to the ~30s shim timeout). When a click's committing event triggered a
**cross-origin top-level navigation**, the renderer **process swap** tore the stale CDP
session down before the mouse-dispatch ack was delivered, so the *dispatch itself*
dead-waited ~30s and surfaced a transport `shim_timeout` / `request_timeout` — **before**
the already-bounded settle ever ran. The caller (an LLM agent) read it as "the click
failed" even though the navigation succeeded. Now the dispatch is bounded by the per-call
deadline and, on an ack lost to a swap, returns a typed *dispatched* outcome so the daemon
proceeds to the bounded settle and returns a typed `settle_outcome` receipt — never a
transport error. `deadline_ms` and `until` remain first-class on `web.click`'s published
schema. Determinism (NFR-DET-01) is preserved: the dispatched-ack-pending outcome hashes
identically to a normal dispatch, so record-time ack timing never perturbs the manifest
hash chain and replay stays bit-equal. (#291)

### Fixed

- **`web.click` / `web.type` / `web.press_key` whose committing input triggers a
  cross-origin top-level navigation no longer surface a transport `request_timeout` /
  `shim_timeout`.** The trusted-input dispatch is now bounded by a per-call budget
  (derived from `deadline_ms`, else the server per-call timeout, and floored above real
  CDP ack latency); a mouse/key ack lost to a renderer process swap yields a typed
  "dispatched" outcome, so the verb proceeds to the bounded post-action settle and returns
  a typed `settle_outcome` receipt for the destination. The click is reported performed;
  only readiness is degraded when the destination is genuinely slow. Host-only change —
  the shim request/response channel is id-multiplexed, so no wire-protocol change was
  needed. (#291)


## [0.15.1] — 2026-08-21 — Cross-origin click settle bounded

v0.15.0 (#285) bounded the post-click settle for **same-origin** navigations, but a
click that triggered a **cross-origin top-level navigation** (e.g. a "Start my free
trial" button that redirects to `checkout.stripe.com`) still surfaced a transport
`request_timeout` — even with `until: "load"` and a generous `deadlineMs` — although
the navigation actually succeeded. The caller (an LLM agent) read the transport error
as "the click failed" and abandoned the flow. Root cause: the shim's standalone
`wait_for` ran three sequential phases (virtual-time arm, settle-with-reattach,
resume) that **each** re-took the full per-phase budget, and a cross-origin renderer
**process swap** means the stale CDP session never observes `virtualTimeBudgetExpired`
or the destination's load event — so every phase dead-waited its full budget (~2–3×)
and blew past the RPC per-call deadline. Compounding it, the daemon's carefully
clamped settle budget never actually reached the shim. Now the whole shim `wait_for`
runs under **one shared wall-clock deadline** and the budget is threaded end-to-end,
so a completed cross-origin navigation returns a bounded, typed `settle_outcome`
(`reached` | `timeout` | `dom_unstable`) inside the deadline — never a transport
error. Determinism (NFR-DET-01) is preserved: `settle_outcome` stays off the manifest
hash chain and replay remains bit-equal. (#288)

Also unbreaks `main` CI: a newer-stable clippy lint (`as_chunks`) and a stale vendored
WASM guest that had left CI red since before v0.15.0. (#289)

### Fixed

- **`web.click` / `web.type` / `web.wait_for` on a cross-origin top-level navigation
  no longer surface a transport `request_timeout`.** The shim's post-action settle now
  shares ONE wall-clock deadline across its virtual-time-arm, reattach, and resume
  phases (previously each re-took the full budget), and the daemon's settle budget is
  threaded to the shim via a new internal `budget_ms` on the wait-for request. A
  completed cross-origin navigation returns a bounded, typed `settle_outcome` receipt;
  `until: "load"` resolves as soon as the destination's load completes. (#288)
- **The settle budget is clamped strictly inside the effective per-call deadline**
  (`min(caller deadline_ms, server per-call timeout)`) with headroom reserved for the
  post-settle virtual-time resume, so a caller `deadline_ms` larger than the server cap
  — or a near-exhausted deadline — can never hand the shim a budget the RPC race would
  beat. Overflow- and zero-budget-guarded. (#288)
- **`main` CI restored to green** — a latest-stable clippy `as_chunks` lint on
  pre-existing audio code and a stale `loom-cli/vendor/loom_surface_web.wasm` (both
  independent of any recent feature work) had left CI red since before v0.15.0. (#289)

## [0.15.0] — 2026-08-20 — Bounded interactive-verb settle

`web.click` and `web.type` now run a **bounded** post-action readiness wait — the
same settle machine `web.navigate` uses — and expose an `until` knob to control it.
Driving a live single-page app whose post-click page never quiesces (a button that
POSTs then redirects to a third-party checkout, a client-route change that keeps a
request in flight) previously could exceed the loom-mcp `tools/call` deadline and
surface as a transport `rpc timeout`, leaving the caller with an error instead of a
receipt. The interaction now always returns a bounded `settle_outcome` receipt
(`reached` | `networkidle` | `timeout` | `dom_unstable`) inside the RPC deadline —
never an `rpc timeout` — with the input dispatch always reported as performed; only
the readiness is degraded. Determinism is preserved: the settle verdict rides
observationally on the receipt and is excluded from the manifest hash chain
(NFR-DET-01), so replay stays structural. Implemented entirely host-side — no WIT
interface change and no vendored-WASM rebuild. (#285)

### Added

- **`until` on `web.click` / `web.type`** — `load` | `networkidle` | `settled`
  (default `settled`), mirroring `web.navigate`. A caller driving a churny SPA can
  request `until: "load"` to proceed as soon as the load event fires without waiting
  for full quiescence. The receipt carries `settle_until` + `settle_outcome`. (#285)

### Fixed

- **Interactive verbs no longer surface a transport `rpc timeout` on unsettleable
  pages** — the post-action settle wait is bounded strictly inside the RPC deadline
  (reusing the wall-clock-bounded `wait_for_settle` path), so a never-settling SPA
  returns a degraded `settle_outcome` receipt instead of failing the whole call. The
  budget honors a caller's `deadline_ms` (subtracting elapsed input-dispatch time;
  `deadline_ms: 0` means "no deadline" per convention) and can never approach the
  transport timeout. (#285)

### Changed

- **`web.click` / `web.type` now settle by default** — previously they returned
  immediately after the input dispatch; they now perform a bounded `settled` wait so
  the readiness contract matches `web.navigate`. Pass `until: "load"` to opt out of
  full quiescence. `web.type mode:"value"` (legacy guest path) is unaffected and does
  not settle. (#285)

## [0.14.1] — 2026-08-17 — Dependency & Security Refresh

A patch release with no feature changes: it clears two Wasmtime security
advisories and refreshes the dependency tree across the board. **RUSTSEC-2026-0222**
(store type-index confusion between engines) and **RUSTSEC-2026-0223** (VM-state
corruption from preemption during bulk operations) are fixed by moving wasmtime
from 46.0.1 to 46.0.2 and then to **47.0.3** — a major-version engine bump that
required no code changes and keeps the replay hash chain byte-equal (the full
determinism suite passes unchanged). The workspace also moves to wit-bindgen 0.60
(byte-identical guest output), jsonschema 0.49, tokio-tungstenite 0.30, wasmparser
0.255, base64 0.23, and ulid 3.0 (internal `Ulid::new()` → `Ulid::generate()`
rename), plus all remaining non-major bumps and updated CI actions.
(#265, #266, #267, #270, #271, #272, #281)

### Fixed

- **cargo-deny advisories** — RUSTSEC-2026-0222 / RUSTSEC-2026-0223 (Wasmtime)
  cleared via wasmtime 46.0.2 → 47.0.3. (#271, #281)
- **Vendored WASM guest staleness** — `loom-cli/vendor/loom_surface_web.wasm`
  refreshed to match current stable rustc output on Linux CI. (#271)

### Changed

- **Dependency refresh** — wasmtime + wasmtime-wasi 47.0.3, wit-bindgen 0.60,
  jsonschema 0.49, tokio-tungstenite 0.30, wasmparser 0.255, base64 0.23,
  ulid 3.0, and workspace-wide non-major `cargo update`. No public API changes.
  (#271, #281)
- **CI actions** — setup-node 7, setup-python 7, and non-major actions-group
  bumps. (#266, #267, #270, #272)

## [0.14.0] — 2026-07-14 — Browser Voice-Call I/O

A minor release that gives loom **ears and a voice**: an agent can now drive a live
browser voice call end to end. A session opts in with `loom session create --audio`,
which installs a **synthetic microphone** in the page (a `getUserMedia` override — the
real mic is never delegated, so host audio cannot leak). From there, `web.inject_audio`
pushes caller audio into that mic, `web.start_audio_capture` / `web.stop_audio_capture`
record the inbound side of the call into a bounded WAV blob, and `web.say` speaks text
straight into the mic through a configured external TTS backend. Captured audio is
retrieved with the new `loom blob get`, and both SDKs expose the verbs plus fetch/save
helpers.

Audio is a **daemon-side side-channel** — intercepted in `wasm_bridge` like
`web.start_recording` — so there is no WIT change and no vendored-wasm rebuild. The
audio verbs require `--no-determinism` (a live call cannot run on a frozen virtual
clock), and audio bytes are excluded from the manifest hash chain: only a constant
outcome hash and a content hash reach the receipt, so replay stays byte-equal
(NFR-DET-01 holds). (#243, #249, #251, #252, #253, #254, #255, #262, #263)

### Added

- **`web.inject_audio`** — inject caller-provided audio (a CAS `blob_ref` or inline
  base64, ≤ 8 MiB) into the page's virtual microphone. Bytes are passed as a CDP
  `CallArgument`, never string-interpolated into the page. (#252)
- **`web.start_audio_capture` / `web.stop_audio_capture`** — capture the inbound call
  audio (the remote party) into a bounded in-page ring buffer, then drain it to a
  16 kHz mono WAV. Byte and duration caps compose (the tighter one wins), and the stop
  result carries a typed `audio_stop_reason` (`byte_cap` / `duration_cap` / …) so a
  truncated capture is never silently mistaken for a complete one. The agent's own
  injected track is excluded from the capture. (#253, #254)
- **`web.say`** — synthesize text to speech and inject it into the microphone in one
  verb. The backend is configured out-of-band via `LOOM_TTS_CMD` (argv-exec: text on
  stdin → WAV on stdout) or `LOOM_TTS_URL` (https POST, SSRF-guarded — https-only,
  redirects refused, pinned resolution). (#263)
- **`loom session create --audio`** — per-session opt-in that installs the synthetic
  mic and grants microphone permission scoped to the navigated origin (revoked on
  close). (#251)
- **`loom blob get <hash>`** — retrieve any stored blob from the content-addressed
  store to a file (`-o`) or stdout (`-o -` / a pipe); refuses a bare TTY. (#262)
- **SDK audio verbs (TypeScript + Python).** `injectAudio` / `startAudioCapture` /
  `stopAudioCapture`, the `audioAfterHash` / `audioStopReason` receipt fields, and the
  `fetchAudioCapture` / `saveAudioCapture` retrieval helpers. Both SDKs warn when a
  capture was truncated by a cap. (#262)

### Changed

- **wasmtime / wasmtime-wasi 45 → 46.0.1.** (#250)

### Fixed

- **Toolchain-drift clippy lints and a stale vendored wasm** on `main`. (#244)

## [0.13.4] — 2026-06-25 — `web.wait` Accepts the Locator Grammar

A patch release that closes the last gap in loom's locator grammar: `web.wait --selector`
now accepts the same `css=` / `text=` / `role=` / `frame=` grammar that `web.click`,
`web.type`, and `web.set_input_files` already resolve. Previously `web.wait --selector
"text=Ready 1"` threw `js_throw` (the daemon passed the raw locator straight to
`document.querySelector`, an invalid CSS selector → `SyntaxError`), while the very same
locator clicked fine — a confusing asymmetry for agents. `web.wait` is now intercepted
host-side like `web.click` and polls the resolved locator until it matches or `timeout_ms`
elapses, which also makes real the polling the docs already promised (the old path was a
single probe). A bare value still resolves as CSS (back-compat); a timeout still surfaces
the typed `kind: "wait_predicate_false"`. Host/daemon-side only — no WIT, vendored-wasm, or
hash-chain change; only the resolved/timed-out verdict is recorded (never poll count or
timing), so replay stays byte-equal (NFR-DET-01). (#233)

### Added

- **Locator grammar for `web.wait`.** `web.wait --selector` resolves `text=` (visible text),
  `role=` (ARIA role + accessible name), `css=`/bare (CSS, presence), and a `frame=` prefix
  (same-process iframe) — the same grammar as `web.click`/`web.type` — and polls the locator
  (100 ms cadence, default 30 s timeout clamped to 600 s) until it resolves. (#233)

### Fixed

- **`web.wait` no longer throws `js_throw` on a non-CSS locator.** `text=`/`role=`/`css=`
  forms reached Chrome as literal (invalid) CSS selectors and raised a `SyntaxError`; they
  now route through the host-side locator resolver, matching `web.click`. (#233)

## [0.13.3] — 2026-06-25 — `no_determinism` Sessions Stop Wedging on Cross-Origin Auth'd SPAs

A patch release that unblocks driving real authenticated SPAs under `--no-determinism`
(real wall-clock — the mode MCP/agentic clients use). The complement to v0.13.2's fix:
that release fixed the navigate-*exit* virtual-clock resume under `--no-determinism`;
this one fixes the *arm* side. A `--no-determinism` session navigating a second time to
a heavy app behind a cross-origin login (e.g. an Auth0 New Universal Login with a hidden
`prompt=none` silent-auth iframe) hung the full deadline and left the session unusable
(every later verb `surface_trap`ped). Root cause: the virtual-time arm/await was gated on
the process-global capture flag (`virtual_time_enabled()`, on by default) rather than on
whether the session actually pinned the clock at inject — so a `--no-determinism` session,
which never pins the clock, still armed a virtual-time budget and then awaited a
`virtualTimeBudgetExpired` that can't fire on a real wall-clock (a cross-origin iframe
keeps network fetches pending under `pauseIfNetworkFetchesPending`, so the budget never
expires). Host-side shim fix only; no WIT, vendored-wasm, or hash-chain change, and the
determinism-pinned path is byte-identical, so replay stays byte-equal (NFR-DET-01). (#230)

Also adds the ability to request `no_determinism` through the MCP implicit-session path
so MCP clients can drive real auth'd SPAs (whose bootstrap timers never fire under the
frozen clock) on a real wall-clock. (#229)

### Added

- **`no_determinism` on the MCP implicit-session path.** A new
  `LOOM_MCP_SESSION_NO_DETERMINISM` env baseline plus a `no_determinism` boolean on the
  `session.create` tool and `session.reset`, merged over the env baseline like the other
  knobs and forwarded to `session.create`. The key is emitted only when enabled, so the
  default create shape is byte-identical to before; a typo'd boolean is a hard startup
  error (determinism is never silently left on). Such a session is recorded
  non-replayable, as on the CLI path. (#229)

### Fixed

- **`--no-determinism` navigate/`wait_for` no longer arms (and then hangs on) virtual
  time.** The arm/await is now gated on whether the session actually pinned the clock at
  inject (`determinism_injector::clock_pinned`, recorded only when the freeze-inject runs
  — i.e. under `determinism_enabled`) instead of the env-global capture flag. A
  `--no-determinism` session takes the clean real-clock load+settle path, so a repeated
  navigate to a cross-origin-iframe SPA (the Auth0 silent-auth case) settles promptly
  instead of wedging the session on the second navigate. Determinism-ON behavior is
  byte-identical (the new gate equals the old one whenever the clock is pinned), and
  `--no-determinism` sessions are non-replayable, so the replay hash chain is unchanged
  (NFR-DET-01). (#230)

### Changed

- **The shim is now observable when it fails.** `loom-host` drains the
  `loom-shim-chromium` subprocess's stderr into the daemon log (it was captured but never
  read, so a shim panic surfaced only as an opaque `ExitStatus(256)`), and the shim gained
  a `RUST_LOG`-gated stderr tracing subscriber (it previously had none). Diagnostics only;
  silent by default. (#230)

## [0.13.2] — 2026-06-24 — Repeated `web.navigate` No Longer Degrades the Session

A patch release that fixes a session-wedging regression: within a single session, each successive
`web.navigate` took ~20s longer than the last, and by the ~4th call it exceeded the 30s deadline and
hung — the `loom-shim-chromium` subprocess then exited, leaving an orphan Chromium, and every later verb
returned `surface_trap`. It reproduced under `--no-determinism` even on a trivial page like
`example.com`, so it blocked any multi-step agentic journey that navigates more than a few times. The
root cause was the navigate-exit virtual-clock resume guard being too broad: with virtual-time capture
on (the default), each navigate left the renderer's virtual clock paused at the drained budget horizon,
and under `--no-determinism` — where there is no replay contract to preserve — that paused clock deferred
the next navigate's `Page.loadEventFired`, so each call burned its full settle budget. Host-side shim
fix only; no WIT, vendored-wasm, or hash-chain change, and the determinism-pinned path is byte-identical,
so replay stays byte-equal (NFR-DET-01). (#227)

### Fixed

- **Repeated `web.navigate` in one session no longer degrades, then wedges, the session.** Virtual-time
  capture is on by default and independent of `--no-determinism`, so `page_navigate` arms and drains a
  per-navigation virtual-time budget on every call. The exit-resume guard left the renderer's virtual
  clock frozen on the clean-drain path (`!budget_drained`) to keep a later `web.evaluate` clock read
  replay-equal — correct under determinism, but too broad under `--no-determinism`, where a paused
  virtual clock defers the next navigate's `Page.loadEventFired`. The determinism-OFF navigate path
  awaits load before re-arming a budget, so it deadlocked on the deferred load and burned the full
  navigate budget every call, compounding until the 30s deadline wedged the session. The renderer is now
  resumed (`setVirtualTimePolicy{advance}`) on navigate exit unless on the determinism-pinned clean-drain
  path (`clock_pinned && budget_drained`), so every navigate starts on an advancing clock and settles
  promptly. The fix is provably a no-op under determinism (the new guard is algebraically identical to
  the old one whenever the clock is pinned), so replay-equality is untouched. (#227)

## [0.13.1] — 2026-06-23 — `set_input_files` Locator Grammar

A patch release that brings `web.set_input_files` in line with `web.click`/`web.type`: the
upload verb now parses the **locator grammar** (`css=` / `frame=` / …) on its `--selector`
instead of passing the raw string to `DOM.querySelector`. Before this, a `css=`-prefixed
selector — the documented way to write one — never resolved, so a valid file under a valid
`LOOM_UPLOAD_ROOT` surfaced (under `--json`) as the opaque `surface_trap: action dispatch
failed`. Now `css=` works, `frame=<css> >> css=<inner>` can upload into a same-process (incl.
same-site cross-origin) iframe, and a genuine no-match returns the typed `selector_not_found`.
Host-side only — no WIT, vendored-wasm, or hash-chain change; replay stays byte-equal
(NFR-DET-01). (#225)

### Fixed

- **`web.set_input_files` now accepts the locator grammar (`css=` / `frame=`).** The verb passed
  its `--selector` straight to `DOM.querySelector` and never ran the locator-grammar parser that
  `web.click`/`web.type` gained in #223. So a `css=`-prefixed selector — the documented way to
  write one — never resolved, and a non-resolving selector surfaced (under `--json`) as the
  opaque `surface_trap: action dispatch failed` (via `ShimFailure → SurfaceTrap`). The result:
  `set_input_files --selector "css=#upload"` trapped even for a valid file under a valid
  `LOOM_UPLOAD_ROOT`. It now resolves the selector through the same frame-aware resolver
  click/type use — so `css=` works and `frame=<css> >> css=<inner>` can upload into a
  **same-process (incl. same-site cross-origin) iframe** — and a genuine no-match returns the
  typed `selector_not_found` outcome. The `LOOM_UPLOAD_ROOT` allow-list (the file-path boundary)
  is unchanged. Host-side only: no WIT, vendored-wasm, or hash-chain change; replay stays
  byte-equal (NFR-DET-01).

## [0.13.0] — 2026-06-23 — Interact Inside Cross-Origin Iframes + text=/role= Locators

A minor release that lets the interaction verbs reach **inside a cross-origin iframe** and
address controls by **visible text / ARIA role** — the two limitations that blocked driving a
real app whose "test the bot" surface is an embedded widget. loom could already *read* a
cross-origin iframe (`web.snapshot --pierce`), but `web.click`/`web.type` were top-frame-only:
the parent's `iframe.contentDocument` is `null` cross-origin, so a composer/Send selector
resolved to nothing. This adds a small **locator grammar** on the existing `--selector` string
and makes click/type descend into a same-process (incl. same-site cross-origin) frame. Verified
against **real Chrome** via a hermetic two-port `127.0.0.1` fixture. Replay stays structural and
byte-equal (NFR-DET-01) — the grammar is parsed for resolution only; the raw selector still
feeds the manifest hash, so plain CSS selectors are unchanged and there is no WIT, vendored-wasm,
or hash-chain migration. (#223)

### Added

- **Locator grammar on `web.click` / `web.type` (#223).** The `--selector` accepts composable
  segments joined by ` >> `: `css=<selector>` (the default for a bare selector), `text=<visible
  text>` (case-insensitive substring, first visible match), `role=<role>[name="<accessible
  name>"]` (ARIA role + a W3C accessible-name subset: aria-label → aria-labelledby → associated
  label/placeholder → text → title), and `frame=<css>` to descend into an iframe.
- **Cross-origin iframe interaction (#223).** `frame=<css> >> css=<inner>` resolves and
  trusted-clicks / types into an element inside a same-process (incl. same-site cross-origin)
  iframe — descending via `DOM.describeNode{pierce} → contentDocument`; `getBoxModel` returns
  top-level viewport coordinates, so the dispatch lands unchanged.
- **Cross-origin scope fence (#223).** A bare locator (no `frame=` segment) resolves only in the
  top frame + same-origin subframes; crossing an origin boundary **requires** an explicit
  `frame=` segment, so a bare `text=`/`role=` can never dispatch trusted input into an arbitrary
  third-party iframe.

### Changed

- `loom action web.click`/`web.type --help` now document the locator grammar; `docs/` regenerated.
- Docs: clarified that `web.wait` cannot drive a navigation — use `web.wait_for` (#222).

Follow-ups (tracked, not in this release): re-arming the virtual-time budget for non-navigation
click-triggered async (the click analogue of #219's `web.wait_for` fix); `text=`/`role=` *inside*
a frame; and true cross-site out-of-process iframes (the Mentiora widget is same-site, so it is
already covered by the in-process path).

## [0.12.4] — 2026-06-23 — web.wait_for Drives the Submit to Completion (Auth0 New ULP)

A patch release that makes a click-triggered form **submit actually navigate**. On Auth0's
New Universal Login (identifier-first), `web.type` email → `web.click` Continue →
`web.wait_for` left the page on `/u/login/identifier` — no POST, no navigation,
`location.href` unchanged (the last blocker after #216 landed the input fill). loom drives
Chromium under a **deterministic virtual-time clock** that is *frozen* after a
navigate/settle drains its budget; interaction verbs (`web.click`/`web.press_key`) are raw
`Input.*` CDP passthroughs that arm no budget, and `web.wait_for` re-armed one only AFTER a
navigation had already begun. So a click-triggered async `onSubmit` chain (react-hook-form:
async validate → `navigator.credentials` probe → `fetch(POST)`) stalled at its first
`setTimeout`/macrotask `await` and never issued the request. Verified live against a real
Auth0 New ULP tenant: the identifier step now advances to `/u/login/password` and the
password step issues its `POST /u/login/password`. Replay stays structural and
value-independent (NFR-DET-01). (#219)

### Fixed

- **`web.wait_for` now arms a bounded virtual-time budget at the start of its settle
  (#219).** Under the determinism clock pin the virtual clock is frozen once the prior
  navigate's budget drained, so a preceding `web.click`/`web.press_key` that scheduled async
  work behind a timer never advanced — the page never began the navigation `wait_for` was
  meant to observe, and the old common path settled the still-`complete` document
  immediately. `wait_for` now arms the same `pauseIfNetworkFetchesPending` budget `navigate`
  uses (mirroring its STEP 4c) before settling, draining pending timers so the submit chain
  reaches its `fetch` and any resulting top-level navigation begins; the existing
  settle/reattach then resolves it on the new document. Shim-side only (loom-shims
  `action_executor`) — no WIT/guest/vendored-wasm change. The virtual-time control commands
  are shim-internal and excluded from the manifest hash, so the chain stays replay-equal.

## [0.12.3] — 2026-06-23 — web.type Drives React Controlled Inputs (Playwright fill)

A patch release that makes the **default `web.type` log into real React apps**. Auth0's
New Universal Login (identifier-first) uses **react-hook-form**, and neither prior dispatch
path was browser-equivalent: the old default `value` mode set `.value` + synthetic
`input`/`change` events (`isTrusted:false`) — it reaches the framework's value-tracker but
the submit treats it as *not genuinely entered* (no POST, silent no-op); `mode:"keystrokes"`
(added in 0.12.1) is genuine but its value never lands in react-hook-form state ("Please
enter an email address"). A real browser / Playwright `fill()` is **both**. Replay stays
structural and value-independent (NFR-DET-01). (#216)

### Changed

- **`web.type`'s default mode is now `fill`, driven by CDP `Input.insertText` (#216).** Bare
  `web.type` focuses the element, selects its existing content, and commits the text via a
  single `Input.insertText` — the mechanism Playwright `fill()` uses. It produces one genuine
  (`isTrusted:true`) `beforeinput`/`input` event through Chromium's editing pipeline, so
  React/react-hook-form `onChange` fires AND the value is treated as user-entered; the
  identifier/login flow advances. `text:""` clears the field. The legacy native-setter path
  is preserved as the explicit `mode:"value"` escape hatch; `mode:"keystrokes"` is unchanged;
  an unrecognized mode behaves as `value`. Host-side only (mirrors the keystrokes intercept) —
  no WIT/guest/vendored-wasm change; the manifest hash chain stays replay-equal.

## [0.12.2] — 2026-06-22 — web.get_cookies Returns Cookies

A patch release that makes **`web.get_cookies` actually surface the cookies it reads**.
Cookie *persistence* already worked — server `Set-Cookie` cookies (including `HttpOnly`)
are stored and transmitted on subsequent requests within a session — but `get_cookies`
forwarded `Network.getCookies` through the opaque guest path, and the WASM guest ships
without a CBOR decoder, so the decoded cookie array was never put on the receipt (every
receipt hardcoded `get_cookies_result: None`). Agents driving cookie-dependent logins
(OAuth/OIDC, CSRF-protected forms) saw "0 cookies" and assumed cookies weren't kept.
Replay stays structural and value-independent (NFR-DET-01). (#212, #214)

### Fixed

- **`web.get_cookies` now returns the cookie array on the receipt (#214).** The host
  decodes the `Network.getCookies` response in `shim_call` (the guest can't) and moves it
  onto `get_cookies_result` with raw values (operator-facing, D7); cookie *values* are
  redacted from the manifest hash chain via a re-derived `outcome_hash`, so replay stays
  structural and cross-run value-independent. `HttpOnly` cookies — invisible to
  `document.cookie` — now surface in full with their CDP fields. Host-side only; no
  WIT/guest/vendored-wasm change. `set`/`clear`/`delete` cookies are unchanged.
- **Screencast `stop_reason` survives an encoder failure (#212).** A stop that races a
  failed ffmpeg encode now reports the truthful stop reason, and the cap-enforcement e2e
  tests are decoupled from the flaky ffmpeg-sidecar runtime download.

## [0.12.1] — 2026-06-22 — Trusted CDP Input

A patch release adding a **real-CDP-input path** so loom can drive `isTrusted:true`
browser input that passes trust-gating frameworks (notably Auth0 New Universal Login,
which ignore synthetic events). Plus a settle-driver fix for client-initiated
navigations and a Rust CI compile-time speedup. All changes preserve the replay-equal
hash chain (NFR-DET-01) — real input changes record-time fidelity only; replay stays
structural. (#206–#210)

### Added

- **Real CDP input dispatch (#207).** `web.type` gains `mode: "value"` (default — the
  existing native-setter path) `| "keystrokes"` (focus the element and send a real
  per-character `Input.dispatchKeyEvent` sequence). New **`web.press_key`** verb — named
  keys (Enter/Tab/Escape/arrows/…) plus modifier combos (Control/Alt/Shift/Meta) and an
  optional `selector` to focus-then-press. These dispatch through Chrome's real input
  pipeline, so the events are `isTrusted:true` and pass frameworks that gate on trust.
  Routed host-side (no WIT/guest/vendored-wasm change); determinism preserved via a fixed
  US keymap + a constant `outcome_hash` dispatch marker.

### Changed

- **`web.click` is now always trusted (#207) — BREAKING.** It resolves the element's hit
  point and dispatches a trusted `Input.dispatchMouseEvent` (mouseMoved → pressed →
  released) instead of a synthetic `el.click()`. Scripts that clicked covered / zero-size
  / `display:none` elements (which `el.click()` could reach) now receive a clear
  `status:error` ("element not hittable: no box model").
- **Faster Rust CI (#208)** — a `ci-fast` compile profile (no fat LTO) + the `lld` linker
  cut whole-PR Rust CI time. (Internal; no runtime effect.)
- **Internal: shared `settle_with_reattach` helper (#210)** — factored out of `page_navigate` + `wait_for`; no behavior change.

### Fixed

- **Settle: re-attach to client-initiated top-level navigations (#206)** — SPA redirects
  and form-POST navigations now settle and capture correctly.

## [0.12.0] — 2026-06-19 — Video Capture + Per-Action Deadlines

A feature-and-hardening minor. The headline is **browser video/screen capture** — record
a target or a whole session to WebM through the shim, with a byte/duration cap enforced
shim-side. Alongside it: **per-action deadlines** honored end-to-end over MCP/RPC (the
daemon kills the in-flight action and returns a typed `request_timeout`), an opt-in
**content-bearing `dom_after_hash`** fingerprint tier for interactions, and a sweep of
capture/network/scroll fixes. Internally, the **test suite is now parallel-safe** under
cargo-nextest (the `--test-threads=1` mandate is retired). All changes preserve the
replay-equal hash chain (NFR-DET-01). (#178–#204)

### Added

- **Browser video / screen capture (#196).** New `web.start_recording` / `web.stop_recording`
  surface verbs (plus a whole-session recording mode) stream `Page.screencastFrame` JPEGs
  through the shim and encode WebM, with a configurable byte/duration cap enforced before
  encode. A kill-switch (`LOOM_DISABLE_RECORDING`) refuses recording cleanly.
- **Per-action deadlines over MCP/RPC (#202).** `deadline_ms` is now honored end-to-end:
  the daemon enforces it server-side, kills the in-flight action when it expires, and
  returns a typed `request_timeout` instead of hanging the client.
- **Content-bearing `dom_after_hash` for interactions (#197),** behind an opt-in
  fingerprint tier, so interaction receipts can carry a content hash of the post-action DOM.
- **`recreated_count` in `loom.session.info` (#190)** — surfaces how many times an
  evicted session was implicitly recreated.
- **Configurable per-CDP-command navigate budget** via `LOOM_SHIM_CDP_TIMEOUT_MS` (#187).
- **`budget` arg on `loom.session.reset` (#185),** mirroring `session.create`.

### Fixed

- **`web.scroll` scrolls the document/viewport and records `scroll_result` (#195).**
- **Reliable framer-motion reveal capture (#194)** — un-wedge the page, honor a screenshot
  deadline, and capture `whileInView` reveals.
- **`network_events` + HAR populate method/status/sizes (#192);** byte sizes stay out of the
  hashed chain under determinism.
- **Per-session in-order serialization guard for concurrent dispatch (#193).**
- **Engine-aware `.cwasm` staleness (#191)** — composite stamp, auto-refresh, doctor flag.
- **Graceful SIGTERM shutdown unlinks the daemon socket (#188).** `SocketServer` holds an
  RAII guard that `remove_file`s `loom.sock` on shutdown (and panic); the startup
  stale-socket reclaim now logs a `WARN`.
- **Structured error context surfaced in `TypedReceipt.data` over MCP (#184).**

### Changed

- **Parallel-safe test suite via cargo-nextest (#204).** The `--test-threads=1` mandate is
  retired; the suite runs process-per-test (env/umask/OnceLock/tracing-callsite races fixed).
  `cargo nextest run` is the canonical runner; plain `cargo test` also passes at default
  parallelism. No production behavior change.
- Internal refactors with no behavior change: split four oversized files + collapse
  `too_many_arguments` (#203, #201, #200), `CreateSessionParams` for `create_session_raw`,
  hermetic per-test `TempDir` for loom-core (#198), de-flake the pipelined-hello test (#189),
  delete the dead `llm_cache` subsystem (#186).

## [0.11.1] — 2026-06-11 — Embedded-First Schema Validation

Patch release fixing a v0.11.0 regression where MCP `tools/call` rejected the
documented `until`/`timeout_ms` args on `loom.web.navigate` with a misattributed
`schema_violation` (`field 'params' expected field_unknown got object`) while
`web.wait_for` accepted the same params. Root cause: the daemon validated against
per-method schema files on disk that `loom postinstall` never content-refreshes, so a
pre-settle-capture `web.navigate.json` survived upgrades; #152's fail-closed validation
then enforced the stale schema. (#176, fixes #175)

### Fixed

- **Embedded-first schema validation (#176).** The daemon now validates builtin action
  methods against the schemas compiled into the binary (`BUILTIN_SCHEMAS` moved to
  `loom-shared`) — binary version == schema version, always. Disk schema files act only
  as an overlay for methods unknown to the binary (compiled fail-closed); a stale
  builtin mirror is logged with remediation and ignored. The exact regression
  (`web.navigate` + `until`/`timeout_ms` over MCP) is pinned by a class-level test that
  validates EVERY builtin action method against its full documented arg set.
- **`schema_violation` errors name the real field, the method, and the allowed
  properties** — e.g. `schema violation: web.navigate: unknown field 'bogus' (schema
  allows: session, timeout_ms, until, url)` — instead of blaming the envelope word
  `params`. The error `data` block gains additive `method` and `allowed` fields.
- **`loom postinstall` refreshes stale schema mirrors** (atomic write); the receipt
  reports a `refreshed` count alongside the existing `populated`/`skipped` shapes.

### Changed

- Builtin action methods are validated from first boot — the pre-postinstall
  empty-registry validation bypass is gone (fail-closed, consistent with #152).
  Hand-edits to builtin schema mirror files are no longer honored (loud warning;
  overlay extras for unknown methods remain supported). `rpc.schemas`'
  `source_wit_sha256` is now machine-independent for stock installs.

## [0.11.0] — 2026-06-11 — Hardening Sweep: Determinism, Protocol, Security + MCP Determinism Surface

A large correctness, robustness, and security pass: a deep multi-agent audit produced
~100 verified findings that were fixed and regression-pinned, alongside seven
feature/contract specs driven by downstream (agentic-test-studio) needs. The headline
addition is a determinism surface over MCP so agent clients can drive seed/clock-anchor
recording and the record/replay diff oracle without the CLI. All changes preserve the
replay-equal hash chain (NFR-DET-01); the wire stays compatible with deployed ≤0.10.x
clients. (#138–#173)

### Added

- **Determinism + session surface over MCP (#162).** `loom-mcp` now reads
  `LOOM_MCP_SESSION_SEED` / `LOOM_MCP_SESSION_CLOCK_ANCHOR` / `LOOM_MCP_SESSION_PROFILE`
  for the implicit session, self-heals it on idle eviction (recreate + retry once), and
  exposes `loom.session.reset` / `info` / `diff` (+ `validate`/`export`) tools — the
  cross-run regression oracle over MCP. The implicit-session `tools/call` path is now
  allow-listed; control-plane requests (`ping`/`initialize`/`tools-list`) dispatch
  concurrently to avoid head-of-line blocking, while session-mutating tool calls stay
  serialized in submission order (one browser per session).
- **SDK `clockAnchor` / `clock_anchor`** in both SDKs; receipts now expose
  `status` / `error` / result fields so failed actions are distinguishable from
  successes; `ValidationResult.replayable`.
- **Epoch-based guest preemption (#159).** CPU-bound WASM guests are now interruptible:
  abort and budget-kill actually fire against a busy loop (epoch ticker + per-invocation
  deadline; fuel knob wired).
- **Typed `session_cap_exceeded`** with `{active, cap, hint}`; `doctor` reports
  `at_capacity` (warn) instead of failing at peak load; `doctor --daemon-only`.

### Fixed

- **Determinism.** Per-session serialization of manifest hash-chain appends (concurrent
  appends could fork the chain, #140); deterministic vault-audit payloads and trap-receipt
  timestamps; a single truthful receipt per trapped action; per-session RNG harness so
  `--seed` actually isolates concurrent sessions; replay header fidelity
  (budgets/capture-policy) and a coherent replay-close path.
- **Security.** SSRF guard on the `net_request` host primitive — scheme allow-list +
  loopback/private/link-local/metadata block on the resolved address (DNS-rebind
  resistant) + per-hop redirect re-validation; bearer tokens zeroized and redacted;
  keychain `allow_prompt` honored, items pinned this-device-only (no iCloud sync), Linux
  D-Bus owner-pin check and op on one connection; cookie-name RFC 6265 enforcement; path
  normalization; restrictive socket permissions.
- **Protocol.** Connection protocol v2 — concurrent per-connection dispatch so
  `request.cancel` works, `spawn_blocking` with result fencing so timeouts/cancel preempt,
  honored per-action `deadline_ms`, and an opt-in HELLO ack (no more 50 ms/5 s connect
  stalls) compatible both directions; SDK transports parse the daemon's bare auth-failure
  frame; daemon panic hardening (`shard_path`, message truncation on UTF-8 boundaries).
- **Wired-but-dead features.** `wait_for` alias, default `profile`, `session.reap` and
  vault RPCs added to the builtin-method allowlist; MCP `resources/list` + `resources/read`
  wire shapes; `validate --json` emits the full `ValidationResult`; tarball export writes a
  self-describing manifest; `--no-determinism` replay refusals carry the real
  `not_replayable` reason; network modes are honest (`live`-only page traffic).
- **Lifecycle / leaks.** SIGTERM graceful shutdown (daemon + `loom-mcp`); circuit-breaker
  recovery and failure-class-aware shim eviction; CLOEXEC on shim IPC; session-table
  eviction; abort tears down the shim; reaper group-liveness + safe orphan GC.

### Changed

- CI is substantially faster (per-ref cancel groups, build caching, path filters, trimmed
  matrix) (#171). `loom session validate` and other commands follow the D-7 output
  contract: piped/non-TTY emits JSON, TTY emits curated text.

### Known issues (deferred, non-blocking)

- The shim's fixed 10 s per-CDP-command timeout can trap navigates on a heavily-loaded
  host. Minor doc/contract items: checkpoint-cadence config, post-terminal cache
  tombstone, a profile-dir/pkill ordering race.
- The daemon's per-connection concurrent dispatch shares the same-session action
  reordering hazard class as the (now-fixed) MCP path, but it is unreachable by current
  clients (CLI/SDK/MCP are all single-in-flight per connection); a defensive per-session
  serialization guard is a planned follow-up.

## [0.10.1] — 2026-06-10 — Cross-Run Determinism + Session Reaper

Cross-run hash equality lands: two independent fresh recordings of the same actions
on a deterministic page now diff `field_diffs=0`, not just self-replay — making the
manifest hash chain a true "did anything change?" oracle across runs. Plus a session
reaper for long-lived daemons, a batch of determinism-capture fixes, and an
error-code consolidation. All additive; the replay hash chain (NFR-DET-01) is
preserved.

### Added

- **Cross-run determinism (#133).** New `loom session create --clock-anchor <epoch_ms>`
  flag pins the injected browser clock (`Date.now`/`performance.now`) to a fixed epoch
  via the existing `started_at_ms_override` seam, so two fresh `--seed`+`--clock-anchor`
  recordings reproduce an identical `loom session diff` (`field_diffs=0`, incl.
  `dom_snapshot_hash`). Under determinism, DOM capture now also awaits the CDP
  virtual-time budget (`Emulation.virtualTimeBudgetExpired`) before snapshotting, so
  `setTimeout`-driven DOM mutations fire deterministically; the budget is armed
  after `Page.loadEventFired` (hang-safe) with a timeout fallback to the existing
  settle. Composes with `--seed`; `--no-determinism` opts out. Documented recipe +
  bounded-determinism caveat in the README.
- **Session idle-TTL reaper + orphan-Chromium GC (#128).** The daemon reaps idle
  sessions past their TTL and garbage-collects orphaned Chromium processes, surfaced
  via `reap`/`doctor`.

### Fixed

- **Content-stable `dom_snapshot_hash` + replay-equal manifest chain (#129).** Strips
  the ephemeral per-navigation CDP `frameId` from the DOM CBOR and projects ephemeral
  top-level fields out of the chain hash, so byte-identical content hashes identically.
- **Deterministic virtual-time entrance animations (#127).** Client-side entrance
  animations render via deterministic virtual time instead of a frozen clock.
- **Unify `DOM.getDocument` `pierce: true` across all captures (#135).** Consistent
  pierced DOM capture so snapshots don't diverge by capture site.
- **Manifest append O(n²) → cached last WAL line (#131).** The manifest writer caches
  the last WAL line per session instead of re-reading the whole file on every append.

### Changed

- **Consolidated the three `LoomErrorCode` enums into one canonical enum (#125).**
  Removes the hand-mirrored kebab/snake/string drift class; decode routes through the
  canonical enum.
- **Extracted a shared `network_entries` offload-or-inline helper (#126).** The
  ≥64 KB offload + graceful-degrade logic now lives in one place.
- **Removed the dead `loom-surfaces` crate (#137).** Relocated `safety` and
  `cookie_types` into `loom-shared`; no functional change.

## [0.10.0] — 2026-06-08 — Network Entries + Readiness-Gated Capture

Two capture features land together: per-request network entries surfaced from CDP
(for per-test route footprints), and deterministic readiness-gated capture that
waits for a page to settle before snapshotting. Both are additive and preserve the
replay hash chain (NFR-DET-01).

### Added

- **Deterministic readiness-gated capture (settle-capture, #123).** `web.navigate`
  gains an optional `until` readiness mode so a capture can be gated on the page
  reaching a stable state instead of a raw load event, and a new `loom.web.wait_for`
  tool runs a standalone readiness wait on the current page. The receipt carries
  `settle_until` / `settle_outcome` (recorded on the canonical receipt, so replay
  reproduces them) plus host-side `settle_ms` / `network_count_at_settle`
  diagnostics. Readiness is driven by virtual time so it stays replay-equal; a
  per-session `--no-determinism` opt-out is available.

- **Per-request network entries on the navigate receipt + `loom.web.network_log`
  tool (#122).** `web.navigate` receipts now carry an optional `network_entries`
  array — the raw, complete list of requests the navigation made (document +
  xhr/fetch + subresources), each `{url, method, status, resource_type,
  from_cache, request_id, ts_ms}`, sourced from CDP. Previously loom captured
  only the main-document load and never surfaced the HTTP method, forcing
  consumers (e.g. agentic-test-studio's Test Impact Analysis) into a brittle
  in-page `performance.getEntriesByType('resource')` workaround. The new
  `loom.web.network_log` tool returns the session-accumulated entries since the
  last navigate (capturing xhr triggered by clicks). Large lists (≥ 64 KB) are
  offloaded to the content store as `network_entries_blob_ref` (same pattern as
  `return_value_blob_ref`); `network_entries_truncated` flags an incomplete list.

  The list is **observational** — metadata only (never bodies or headers), and
  **excluded from the replay hash chain**: `network_count`, `side_effects`,
  `network_summary`, and the canonical receipt bytes are byte-for-byte
  unchanged, so determinism (NFR-DET-01) is preserved. Capped at 1000 entries
  per session; redirect hops appear as separate entries sharing `request_id`.

## [0.9.9] — 2026-06-04 — MCP Screenshot Delivery

Screenshots captured by `web.screenshot` / `web.navigate` now reach MCP clients
as usable PNG bytes. Capture always worked, but no renderable image ever reached
a consumer: the content-store blob was a double-encoded `CBOR{data:base64}`
envelope, MCP had no image content type, and there was no way to resolve a
screenshot hash over MCP.

### Fixed

- **Screenshots arrive empty at MCP clients (#116).** The content store now holds
  a raw PNG — decoded from the CDP `{data:<base64>}` envelope by a shared
  host-side helper — and `web.screenshot` actually persists its capture via a new
  typed `record-screenshot` host import (previously it stored nothing and emitted
  a dangling hash). Determinism (NFR-DET-01 hash-chain exclusion of screenshots),
  GC reference-walking, and content-addressed dedup are all preserved.

### Added

- **MCP image delivery (#116).** A `tools/call` for `web.screenshot` /
  `web.navigate` now returns an inline `image` content block (base64 PNG)
  alongside the text receipt, and a new `loom://blob/<hash>` MCP resource resolves
  any content hash to its bytes (PNG-sniffed → `image/png`, else
  `application/octet-stream`). Both reuse the existing `content.get` RPC — no new
  daemon RPC.
- **`cut-release` Claude skill + `CLAUDE.md`.** A checklist-driven release runbook
  (version bump across Cargo/README/SDKs/CHANGELOG, then a tag-triggered
  cargo-dist + PyPI + npm + Homebrew publish) so cutting a release is reproducible.

## [0.9.8] — 2026-06-03 — File Uploads

Adds the file-upload web verb. `web.set_input_files` lets agents drive
`<input type=file>` uploads (the test-studio "upload a knowledge file" flow) —
previously impossible, since browsers ignore typed text into file inputs and
page script can't write the read-only `input.files`.

### Added

- **`web.set_input_files` verb / `loom.web.set_input_files` MCP tool (#101).**
  Uploads one or more local files into an `<input type=file>` via CDP
  `DOM.setFileInputFiles`, implemented with the typed host-function pattern
  (the `web.navigate` / `web.evaluate` lineage): the daemon validates paths, the
  WASM guest calls `set-input-files-execute`, and `loom-host` runs
  `DOM.getDocument → DOM.querySelector → DOM.setFileInputFiles`. Params:
  `selector` (CSS Level 3, same semantics as `web.click`) and `paths` (one or
  more absolute host paths; single-file inputs take `paths[0]`). The receipt
  matches the other mutating verbs (tamper-chain `action_hash` / `outcome_hash`;
  no auto-screenshot).
  - **Security — local file reads are gated by a fail-closed `LOOM_UPLOAD_ROOT`
    allow-list.** Unset ⇒ deny all (`upload_root_not_configured`). Paths are
    `std::fs::canonicalize`d (symlink-escape defense) and must resolve under the
    root, else `upload_path_blocked`. Enforced in ALL profiles (not just Safe).
    Per-call caps: 20 files, 100 MiB/file, 200 MiB total
    (`upload_too_many_files` / `upload_file_too_large` / `upload_total_too_large`).
    Non-existent paths → `upload_path_not_found`; selector miss →
    `selector_not_found`; non-file-input target → `not_a_file_input` (typed
    errors, no panics). loom reads only path + metadata, never file content.
    Verified end-to-end against real Chromium in CI (`e2e pinned-chromium`).

### Changed

- Internal: routed `/feature` telemetry to the hosted event-log then removed the
  hosted config from the public repo (#98, #99); bumped cargo-dist 0.31 → 0.32 (#97).

## [0.9.7] — 2026-05-29 — Cookie Injection (ship milestone)

Closes the **web-cookie-injection** deferral list from v0.9.5. The four
MCP cookie verbs are now reachable end-to-end (`tools/list` →
`tools/call` → daemon → WASM verb → CDP → chromium). Downstream
consumers (notably `mentiora-ai/agentic-test-studio`) can pin
`loom-mcp@0.9.7` and ship the `auth_cookie:` frontmatter feature.

> NOTE — **v0.9.6 was never tagged.** The v0.9.6 cookie-injection
> milestone work + the three v0.9.7 follow-ups (grant resolution
> daemon-side, per-cookie validation taxonomy daemon-side, parallel
> startup sweep) shipped together as v0.9.7 after pre-release
> adversarial review surfaced shape-rejection gaps in the dispatcher
> that wanted closing before tag. In-code comments still reference
> "v0.9.6 web-cookie-injection" as the development-cycle name; the
> public release tag is v0.9.7.

### Added

- **Verb-level `execute()` implementations** for the four cookie
  verbs in `loom-surfaces`. SetCookies resolves `CookieSource::Inline`
  directly and `CookieSource::Grant` via the new
  `host::vault_substitute_cookies` host-fn (see below); validates
  per-cookie via `validate_cookie_params`; dispatches CDP
  `Network.setCookies`. GetCookies returns raw values on the
  operator-facing receipt per D7. ClearCookies emits
  `CookiesCleared{target_id, session_id, count_before}` audit
  BEFORE the CDP call (D9 / FND-0050) — `count_before` from a
  synchronous `getCookies` peek. DeleteCookies uses a peek
  before-and-after to determine `matched: bool`.
- **`vault-substitute-cookies` WIT host-fn** (the 11th host-fn).
  Wraps `Vault::substitute_cookies` (shipped v0.9.5). Chokepoint
  where keychain blob bytes briefly cross into WASM linear memory.
  Replay-mode refuses with `HostError::Internal` — replay values
  come from `replay_cookie_values` per §5.
- **4 cookie verbs on `web-surface` WIT export**: `set-cookies`,
  `get-cookies`, `clear-cookies`, `delete-cookies`. `WebSurface`
  trait + `WebSurfaceImpl` extended; `WEB_SURFACE_VERBS` const now
  enumerates 14 entries.
- **4 `Action` enum variants + 4 `Receipt` cookie-result fields**
  in `loom-rpc/host_service_adapter` so cookie verbs cross the
  RPC boundary. `source` and `*_result` fields use
  `serde_json::Value` because `loom-rpc` has no dep on
  `loom-surfaces` for the typed cookie shapes.
- **4 `ActionMeta` entries** in `loom-rpc/action_registry`
  enumerating cookie verbs for `rpc.schemas` / `loom action
  --help` / man-page generation. Alphabetically sorted to satisfy
  the existing registry-sort test.
- **Daemon `WasmBridge` dispatch arms** for the 4 cookie verbs:
  `action_session_id`, `action_verb`, `build_chromium_args`
  (returns None for cookie verbs — they route through the WASM
  guest, not the direct-shim path). Wire-receipt construction
  (`build_navigate_wire_receipt`) carries the 4 cookie-result
  fields.
- **Receipt cookie-result fields + D13 tuple-identity sort** in
  `loom-host::ReceiptMarshaller`. New `assemble_cookies_canonical_bytes`
  function: sorts cookie arrays by `(name,
  domain.unwrap_or_default(), path.unwrap_or_default())` byte-lex
  (matches UTF-16 lex for ASCII per RFC 6265), replaces every
  `value` field with `"[REDACTED]"` before JCS-encoding. This is
  the replay-byte-identity invariant: same `(name, domain, path)`
  tuples → same canonical bytes → same `outcome_hash`, regardless
  of the actual cookie value.
- **Replay cookie value substitution** in `loom-core::replay_engine::
  cookie_replay`: `substitute_cookie_values(action_id, raw_json,
  &ReplayCookieValues)` swaps each cookie's value by tuple-key
  lookup; missing tuple → typed
  `ReplayError::MissingCookieValue {action_id, name, domain, path}`.
  `parse_replay_cookie_values(json_text)` accepts both pipe-keyed
  object `{"name|domain|path": "v"}` and tuple-array
  `[["name","domain","path","value"]]` shapes for hand-authored
  replay fixtures.
- **Path-level cookie redaction at the MCP boundary** in
  `loom-mcp::mcp_observability`. New `COOKIE_REDACTED_TOOL_NAMES`
  const (the 4 cookie verbs) + `redact_cookie_paths_in_place(&mut
  Value)` walker that strips `value` fields from any `cookies`
  array OR `*_cookies_result` array. Reuses the existing
  `redact_vault: bool` toggle. Critical: cookie *names* and
  `error_code` taxonomy strings remain visible; only `value`
  fields scrub.
- **`vault.get_session_context` RPC**: returns
  `{session_id: String, unambiguous: bool}` for the operator's
  current Active session. Daemon picks the most recently created
  Active session; `unambiguous=true` when exactly one Active
  exists; `SessionNotFound` when zero. The CLI uses this so
  `loom vault add --credential-type cookie` binds to the right
  session automatically per D5.
- **`loom vault add --credential-type <oauth|cookie>` CLI flag**.
  `cookie` requires `--from-file` or `--from-stdin` plus
  `--label`. Validates the JSON blob schema
  (`{"schema_version":1, "cookies":[...]}`) before dispatching to
  the daemon. Resolves session via `vault.get_session_context`
  when `--session` is omitted. Sequence:
  `vault.set_secret` (cookie bytes) → `vault.grant` with
  `credential_type:"cookie"` → prints the resulting GrantId for
  the operator to paste into MCP `set_cookies({grant_id})` calls.
  D7 explicit: NO shred-after-read on the input file (operator
  owns the file lifecycle).
- **§10 heap-wipe hardening** in `loom-shared::redacted`. New
  `wipe_string_buffer_in_place(&mut String)` and
  `wipe_byte_buffer_in_place(&mut Vec<u8>)` helpers explicitly
  zero the heap allocation before drop (works around
  `String::zeroize`'s known length-clear-without-overwrite
  limitation). Wired at the CDP-encode boundary in
  `SetCookiesVerb::execute`. Test verifies the bytes-at-pointer
  are zero after the wipe.
- **`cookie_injection_acceptance.rs` e2e test harness** in
  `loom-cli/tests/` (gated by `--features e2e`). Hand-rolled
  stdio MCP JSON-RPC framing (~150 LOC scaffolding, no rmcp dep);
  sync HTTP echo server (~40 LOC) that captures Cookie request
  headers for the navigate-echo test. 10 test cases; tests
  requiring a real chromium shim further gate on
  `LOOM_TEST_CHROMIUM_AVAILABLE=1`; the harness gracefully skips
  when the daemon isn't running.

### Breaking from v0.9.5 — IMPORTANT for downstream pins

- `loom.web.*` MCP tool naming was established in v0.9.5 (dropped
  the redundant `loom.` prefix that earlier versions had).
  v0.9.7 changes nothing here, but downstream consumers pinning
  `loom-mcp@0.9.5` and migrating to `0.9.7` SHOULD double-check
  any string literals referencing the four cookie verb names —
  they MUST be `loom.web.set_cookies`, `loom.web.get_cookies`,
  `loom.web.clear_cookies`, `loom.web.delete_cookies` (full MCP
  wire names, with the `loom.` prefix the dispatcher prepends).

### Security

- **§11 threat-model section** for cookie credentials in
  `security/vault_threat_model.md`: lawful basis (D6 FND-0010),
  retention (D6 FND-0011), session binding (D5), audit-chain
  visibility (names yes, values no), D7 caveats (no
  shred-after-read on operator file; get_cookies operator-facing
  raw values), §10 heap-wipe boundary.
- **`docs/loom-vault-audit.md`** documents `CookiesSubstituted` +
  `CookiesCleared` audit kinds with canonical-bytes shapes.

### Added (post-adversarial-review hardening)

These three items were tracked as v0.9.7 follow-ups during the
v0.9.6 build; they shipped together with the cookie-injection
milestone in v0.9.7.

- **Daemon-side grant resolution for `web.set_cookies`** — the
  dispatcher now resolves `CookieSource::Grant` to `Inline` by
  calling `Vault::substitute_cookies(grant_id, session_id)` before
  the WASM verb runs. Previously the daemon's `build_chromium_args`
  emitted an empty Network.setCookies envelope for grant sources;
  now it sees a fully-resolved cookie array exactly as if the
  operator had passed `CookieSource::Inline` directly. The
  dispatcher uses a **typed `CookieSource` deserialize** so
  malformed `source` payloads (missing tag, unknown variant,
  missing required field) fail closed with `SchemaViolation`
  rather than silently emitting an empty no-op envelope. The
  vault blob is similarly typed (`{cookies: Vec<NetworkCookieParam>}`),
  so a corrupt keychain blob surfaces `InternalError` instead of
  succeeding with zero cookies.
- **Per-cookie validation taxonomy daemon-side** — validation
  failures from `validate_cookie_params` short-circuit to a typed
  `cookie_validation_error` receipt with `detail.code` carrying
  one of the six snake_case taxonomy strings (`name_empty`,
  `name_invalid`, `value_too_large`, `invalid_same_site`,
  `invalid_expires`, `too_many_cookies`) *before* the chromium
  shim is touched. The wire kind matches the existing WASM-side
  `ErrorMapper` so the two layers produce consistent receipt
  shapes. Operators can group validation failures by code in
  dashboards rather than parsing free-text error messages.
- **Daemon-startup parallel manifest sweep** — `StartupManager::
  sweep_manifests` now fans out per-session WAL processing across
  up to 16 worker threads via `std::thread::scope` (capped by
  `available_parallelism`). Per-session isolation is already a
  design property of the sweep, so concurrent processing is safe.
  Single-threaded fast path retained for corpora < 8 sessions or
  when `available_parallelism` reports a single core (avoids
  `thread::scope`'s setup cost). Recovered/crashed counters
  aggregate via atomics; failures via a single Mutex (rare path).

### Deferred (still) to a future release

- **Daemon-layer policy gate.** Per D9 / FND-0021, the verb-level
  `SafetyPolicy::check_*_cookies` stubs remain always-Ok; the
  authoritative gate would land at the daemon-layer dispatcher as
  a separate hardening PR.
- **CHIPS partition cookies** (CDP-pass-through; browser arbitrates).
- **RFC 6265 edge cases** (Domain leading-dot, IP-host cookies).
  CDP-pass-through.
- **Retention TTL on cookie grants** (no auto-expiry in v0.9.7;
  operator manages via `loom vault delete <label>`).
- **RBAC, admin tooling, multi-operator workflows.**

## [0.9.5] — 2026-05-29 — Cookie Injection (scaffolding milestone)

First milestone of the **web-cookie-injection** feature. v0.9.5 ships the
foundation types, vault extension, CDP cookie encoder, and verb-Action
scaffolding for the four upcoming MCP verbs (`web.set_cookies`,
`web.get_cookies`, `web.clear_cookies`, `web.delete_cookies`). The
verb-level `execute()` implementations + daemon dispatch + first
end-to-end stdio MCP acceptance test will ship in v0.9.6 — see the
follow-up tracking ticket.

### Added

- **`loom_shared::Redacted<T>` newtype.** Hides values in
  Debug/Display/Serialize output (all emit `"[REDACTED]"`); normal
  Deserialize. Bound `T: Zeroize` enforced at the struct level so Drop
  can zeroize the inner value. `expose()` and `into_inner()` (via
  `ManuallyDrop`) are the explicit move-out escape hatches.
- **`CredentialType::Cookie`** vault variant. Cookie grants are
  session-bound (D5 / council FND-0008): `Vault::substitute_cookies(grant_id,
  session_id)` rejects on session mismatch with a typed
  `vault_session_mismatch` envelope.
- **`Vault::substitute_cookies` trait method.** Parallel to the existing
  OAuth-header `substitute()` path: keychain blob → `Zeroizing<Vec<u8>>` →
  returned to caller for decoding into the typed `NetworkCookieParam`
  shape. Cookie *names* land in the audit chain (replay-deterministic);
  values never appear in audit / receipts / logs.
- **Per-CredentialType `grant()` policy (D3 simplified).** Replaces the
  prior single-line OAuth check with an explicit `match` on
  `CredentialType`; `OAuth | Cookie` are accepted, `ApiKey | Saml | Basic`
  remain reserved-and-rejected. Error envelope expanded to advertise both
  `oauth2_authorization_code_pkce` and `cookie` in `allowed_types`.
- **`AuditKind::CookiesSubstituted`** with canonical-bytes shape
  `{grant_id, session_id, cookie_names}` — names for replay determinism,
  no values.
- **`AuditKind::CookiesCleared`** with canonical-bytes shape
  `{target_id, session_id, count_before}` — for `web.clear_cookies` audit.
- **CDP cookie wire types** in `loom-surfaces`: `NetworkCookieParam` (13
  fields, input shape) and `NetworkCookie` (15 fields, output shape) per
  the asymmetric Chrome DevTools spec (council FND-0002). Hand-written
  (chromiumoxide banned in `loom-surfaces` per `deny.toml`).
  `CookieSameSite` / `CookiePriority` / `CookieSourceScheme` enums match
  the CDP PascalCase wire format.
- **`CookieSource` enum** (`Inline { cookies }` | `Grant { grant_id }`) —
  type-safe XOR for `set_cookies` input (council FND-0042); replaces
  runtime `Option<>` XOR.
- **`CookieValidationError` typed enum + `validate_cookie_params()`** —
  enforces a 64-cookie cap (DoS guard per council FND-0044); rejects
  empty names, invalid name characters, oversized values (>4096 bytes),
  pre-1970 expires.
- **4 new `CdpMessage` variants:** `NetworkSetCookies`, `NetworkGetCookies`,
  `NetworkClearBrowserCookies`, `NetworkDeleteCookies`. Encode arms
  produce CBOR envelopes following the existing pattern. `expires` is
  `f64` per CDP spec (the encoder's `f64`-ban applies only to mouse
  coordinates).
- **Verb-level safety stubs:** `SafetyPolicy::check_set_cookies`,
  `check_get_cookies`, `check_clear_cookies`, `check_delete_cookies` —
  all always-`None` (allow-all) under both Default and Safe profiles.
  Authoritative gate lives at the daemon layer per the EvaluateVerb
  dead-code pattern.
- **Verb-Action scaffolding:** `set_cookies_verb`, `get_cookies_verb`,
  `clear_cookies_verb`, `delete_cookies_verb` directories with
  `Action` structs and serde round-trip unit tests. Full `execute()`
  + receipt-builder integration deferred to v0.9.6.

### Changed

- **`Vault::grant()` rejection envelope** — `details.allowed_types` array
  expanded from `["oauth2_authorization_code_pkce"]` to
  `["oauth2_authorization_code_pkce", "cookie"]`. ApiKey/Saml/Basic
  callers see the same `vault_credential_type_unsupported` code; the
  advertised allowlist grows by one entry.

### Security

- **`Redacted<T>` heap-wipe caveat (council FND-0001).** Cookie value
  fields are typed `Redacted<String>` (originally `Redacted<Zeroizing<String>>`
  per D12; rolled back when `Zeroizing<T>` was found to not implement
  `serde::Deserialize`). `String::zeroize()` from `zeroize` 1.6+ calls
  `Vec::clear()` which sets length to 0 but does not write zeros to the
  heap buffer — so memory contents may persist briefly after drop.
  Documented in `security/vault_threat_model.md` as a known caveat;
  proper heap-wipe (via `Zeroizing<Vec<u8>>` at the keychain boundary)
  is a v0.9.6 follow-up.
- **Session binding for cookie grants (D5 / council FND-0008).**
  `Vault::substitute_cookies` returns `vault_session_mismatch` when the
  consuming session_id differs from the grant's stored session_id.
  Defends against intra-daemon IDOR.

### Deferred to v0.9.6

The following Phase 2 plan items did not ship in v0.9.5 and are tracked
for the next release:

- Verb-level `execute()` implementations for the four cookie verbs
  (CDP encode + `host::shim_call` plumbing + receipt assembly).
- Daemon dispatch wiring in
  `loom-daemon::WasmBridge::dispatch_action_blocking`.
- Receipt marshaller extensions for cookie tags + JCS sort by
  (name, domain, path) tuple per D13 (council FND-0039).
- Replay-engine `ReplayError::MissingCookieValue` + fixture-based
  byte-identity tests.
- `loom-mcp::mcp_observability` cookie-value JSONPath redaction.
- `vault.get_session_context` RPC for CLI session-id resolution (D5).
- CLI `loom vault add --credential-type cookie` flag.
- First end-to-end stdio MCP integration test at
  `loom-cli/tests/cookie_injection_acceptance.rs`.
- `security/vault_threat_model.md` cookie-credentials section
  (D6 + D7 caveats).
- `docs/audit.md` documentation of new audit entries.

## [0.9.4] — 2026-05-29

Vault hardening release. Replaces the v0.9.3 placeholder `StubKeychain` with
real platform-keychain backends (macOS Security Framework, Linux Secret
Service via gnome-keyring) and lights up direct credential CRUD on the Vault
trait + `loom vault` CLI surface. Daemon startup is now **fail-closed** on
backend init — there is no silent stub fallback. Existing OAuth-grant flows
are byte-identical; the user-visible surface gains five new subcommands
(`vault add`, `vault delete`, `vault list-labels`, `vault diagnose`, plus
`vault add --overwrite` semantics).

### Added

- **Real macOS Security Framework keychain backend.** `security-framework`
  3.x via `loom-keychain/src/macos.rs`. Items default to
  `kSecAttrAccessible = WhenUnlockedThisDeviceOnly` and
  `kSecAttrSynchronizable = false` (prevents iCloud-Keychain sync of stored
  credentials). All `SecItem*` calls are scoped to
  `kSecAttrService = "loom"` — never cross-keychain, never unscoped.
- **Real Linux Secret Service keychain backend.** `secret-service` 5.x via
  the `blocking::*` submodule (sync API matching the trait shape), in
  `loom-keychain/src/linux.rs`. D-Bus owner pinning on init plus
  `NameOwnerChanged` runtime monitoring give hijack-resistance against a
  same-user `org.freedesktop.secrets` impostor; the resulting
  `SecretServiceOwnerChanged` audit event is emitted on observed
  re-ownership.
- **Daemon backend selection via `LOOM_KEYCHAIN_BACKEND`.** Accepts
  `{auto|macos|linux|in_memory|stub}`. Explicit `macos`, `linux`, or
  `auto` (= platform-native) are **hard-fail-closed** on init failure —
  the daemon refuses to start rather than silently downgrade. When the
  variable is **unset**, the daemon defaults to `in_memory` so CI and
  dev-test contexts that don't have a platform keychain daemon running
  can boot without intervention; production deployments must opt in
  explicitly via `LOOM_KEYCHAIN_BACKEND=auto` (or `=macos` / `=linux`).
  The `stub` backend (always-`Err(Unavailable)`) is retained for the
  narrow case of "run the daemon with vault disabled entirely".
- **`loom vault add <label> [--from-stdin|--from-file] [--overwrite]
  [--session]`.** Direct credential injection alongside the existing
  OAuth-grant flow. 1 MiB read cap, binary-safe (no UTF-8 check, no
  trailing-newline strip), bytes travel as lowercase hex over JSON-RPC.
  Default behaviour fails on existing label; `--overwrite` replaces.
- **`loom vault delete <label> [--force]`.** Removes the credential from
  the platform keychain. Without `--force`, refuses if any alive grant
  references the label and returns `VaultRejection { code:
  "credential_in_use", active_grants: N }`. With `--force`, cascade-revokes
  every active grant (one `GrantRevoked { reason: "credential_deleted" }`
  audit entry per grant) before deleting.
- **`loom vault list-labels [--session]`.** Lists stored credential labels
  scoped to `service_id = "loom"`. Distinct from `loom vault list` (which
  enumerates grants).
- **`loom vault diagnose`.** Stable JSON schema
  `{backend, init_status, service_id, label_count, last_keychain_error?}`
  with SemVer stability promise — see `docs/loom-vault-audit.md` for the
  contract.
- **Eleven typed `AuditKind` variants for credential lifecycle.**
  `SecretOpPending`, `SecretStored`, `SecretFetched`, `SecretDeleted`,
  `SecretReplaced`, `SecretsListed`, `SecretStoreFailed`,
  `SecretDeleteFailed`, `SecretFetchFailed`, `PromptBlocked`,
  `SecretServiceOwnerChanged`. Forward-compat is preserved via
  `#[serde(other)] Unknown` so older readers tolerate future variants.
- **Typed `KeychainErrorKind::TimedOut` and `NonInteractivePrompt`.**
  `KeychainErrorReason` is now a closed enum rather than free-form
  `String` (prevents secret-bytes-in-error-message leakage). Internal
  errors carry a `SHA-256(internal_hash)` correlation handle instead of
  the raw upstream message.
- **`LOOM_KEYCHAIN_ALLOW_PROMPT` env var.** Defaults to `0` in non-TTY
  contexts — headless daemons never hang on a biometric/unlock prompt.
  Blocked prompts emit a `PromptBlocked` audit event.
- **`BlockingKeychain` adapter.** Wraps the sync keychain trait with
  `spawn_blocking` + `tokio::time::timeout` (30 s get, 5 s set/delete/list
  per D28); falls through to a direct call when no runtime is present
  (sync unit tests). LocalVault holds `Arc<BlockingKeychain>` per the
  single-owning-module invariant.
- **Defense-in-depth label validation.** Boundary checks at the CLI, the
  daemon RPC, and `manifest_writer::append_audit` all enforce
  `^[A-Za-z0-9:_-]{1,64}$`; any `Secret*` payload with a malformed label
  is rejected with `VaultInvalidLabel`.
- **CI gnome-keyring setup on Ubuntu cell.** Installs `libdbus-1-3
  dbus-x11 gnome-keyring`, starts `dbus-launch`, unlocks the daemon under
  a 60 s `timeout`, and exports `KEYCHAIN_CI_REQUIRE_DAEMON=1` on
  success. Setup failure is best-effort: tests self-skip cleanly rather
  than fail-the-job per FND-0047 (non-blocking on CI infra flake).
- **Hermetic e2e + acceptance suite.** `loom-keychain/tests/keychain_acceptance.rs`
  (6 tests, GREEN) plus the new `loom-cli/tests/keychain_e2e_hermetic.rs`,
  which round-trips `vault add` → `vault list-labels` → restart → re-list
  to prove the daemon is wired to a real backend (not a stub), and
  performs a byte-level scan across the daemon `data_root` to verify the
  per-test canary string is **never** persisted to disk (G1 enforcement).
- **Threat model + operator docs.** `security/vault_threat_model.md`
  extended with G5a/G5b split, AB6 + AB7 abuse cases, TB2 no-cache
  codification, and a 5-row claimed-controls / known-gaps table for
  SOC 2 + ISO 27001. New `docs/PRIVACY-loom-vault.md` (GDPR Art. 6(1)(a)
  explicit-consent framing, retention model, DSAR procedure) and
  `docs/loom-vault-audit.md` (full `AuditKind` reference + `jq` runbook
  recipes + stable `vault diagnose` schema). Regenerated `docs/loom-vault.1`
  covering all seven v0.9.4 subcommands.
- **0600 startup probe + post-write tightening on auth artefacts.**
  `loom-daemon` refuses to start if `hello.token` or `daemon.pid` carry
  loose perms (`g+r|w|x` or `o+r|w|x`); fresh files are tightened to
  `0600` unconditionally so a default `0022` umask cannot leave them at
  `0644`. Unix-only; Windows ACLs out of scope. Crash-only — no
  auto-chmod.

### Changed

- **Unified `KeychainAccess` trait.** Deleted the duplicate
  `loom_core::vault::KeychainAccess` in favour of the single definition
  in `loom-keychain`. `CoreApiFacade::new` now accepts an injected
  `Arc<dyn KeychainAccess>` (previously hardcoded a `NullKeychain`).
- **`loom vault add` requires `--overwrite` to replace.** Was silent
  upsert. Without the flag, an existing label returns
  `VaultRejection { code: "already_exists" }`.

### Fixed

- **`scripts/lint_no_platform_imports.py` SCAN_DIRS.** The v0.9.3 paths
  resolved to a non-existent directory and the lint silently scanned
  zero files. Fixed to point at the real `loom-host/src` and
  `loom-core/src`; backed by a new `scripts/test_lint_no_platform_imports.py`
  that plants a violation fixture and asserts the lint catches it
  (regression guard).

### Notes

- Windows Credential Manager backend remains a follow-up.
- Biometric access control (`LOOM_KEYCHAIN_REQUIRE_BIOMETRY`) is **not**
  in 0.9.4 — tracked as `loom v0.9.x` follow-up.
- macOS `kSecAttrCreator` ownership discriminator + the corresponding
  Vault-side ownership check (AB6 same-user-read full-mitigation) are
  deferred to 0.9.5 (accepted risk in the v0.9.4 band).
- Strict CLI exit-code mapping per D33 (0/1/2/3/4/5) is partial in 0.9.4
  — label/argument failures map to exit 2 via the existing
  `error_mapper`; the dedicated `AlreadyExists` exit-5 path is a 0.9.5
  follow-up.
- `loom vault add --overwrite` TTY × `--yes` interactive prompt matrix is
  deferred to 0.9.5; the wire-level overwrite guard is in place for the
  CI/script path that is the v0.9.4 primary use case.
- First-run consent dialog for credential storage is deferred to 0.9.5
  (needs interactive UX + per-`data_root` "shown once" state); the
  PRIVACY doc documents the policy.
- Peer-UID RPC authentication remains deferred; caller identity continues
  to rely on socket mode `0600` + per-user runtime dir (existing TB3
  boundary).

## [0.9.3] — 2026-05-21

Hygiene + hardening release. Closes one disclosed-known-gap from 0.9.2
(the `daemon.health` amplification path) and rolls up two weeks of
dependency hygiene — most notably the latent tokio sync-primitive fixes
from 1.52.3 that affect mpsc / `RwLock` code paths under concurrent
session load. No user-visible behaviour change to session lifecycle,
navigate, or replay; existing `loom doctor` / `loom action` flows
produce byte-identical output.

### Added

- **Per-connection rate limit on `daemon.health` (#58, #86).** Closes
  the amplification-via-deep-probe gap disclosed in 0.9.1's CHANGELOG
  and flagged as Sec-5 in #56's security council review. Token-bucket
  caps per-connection sustained call rate (10 RPS) with a burst
  tolerance (30 calls). An empty bucket returns the new typed
  `LoomErrorCode::TooManyRequests` (wire string `"too-many-requests"`)
  so clients can back off and retry. Tunable via
  `LOOM_DAEMON_HEALTH_RATE_RPS` and `LOOM_DAEMON_HEALTH_RATE_BURST`.
  Gate fires only on `daemon.health`; all other RPCs are unaffected.

### Changed

- **Dependency hygiene (#84, #85).** 18 cargo bumps + 5 GitHub Actions
  bumps cleared the May 2026 backlog in one pass instead of waiting out
  the Dependabot schedule. Notable changes ride along:
  - `tokio 1.52.2 → 1.52.3` patches four latent bugs in sync primitives
    (mpsc `len()` underflow, `OwnedPermit::release()` receiver-notify,
    `RwLock` `max_readers != 0` precondition, mpsc `try_recv()` on
    closed-with-permits) — loom's daemon RPC dispatch, per-shim IPC,
    and session manager are all mpsc-heavy under concurrent load.
  - `jsonschema 0.46.4 → 0.46.5` patches the validator that runs on
    every inbound RPC.
  - `dashmap 6.1 → 6.2` improvements in the concurrent map used for
    session state.
  - Major-version migrations: `thiserror 1→2`, `rand 0.8→0.9→0.10`
    (incl. the `OsRng → SysRng` rename and the `RngCore → Rng` /
    `RngExt` split), `zip 2→8`, `toml 0.9→1.1`, `wit-bindgen 0.40→0.57`
    (refreshed vendored wasm), `sha2 0.10→0.11` (migrated 4 call sites
    to `hex::encode`), `jsonrpsee 0.24→0.26`, `chromiumoxide 0.6→0.9`,
    `serde_jcs 0.1→0.2`, `addr2line 0.24→0.26`, `gimli 0.31→0.33`,
    `clap_mangen 0.2→0.3`. All behaviour-preserving — the rand
    cluster's ChaCha20 stream is bit-for-bit identical given the same
    seed, the JCS canonical output is unchanged, all receipt hashes
    are stable.
- **CI cost-cuts (`8185783` and earlier).** Workflow-level concurrency
  cancels superseded PR runs, `beta` toolchain dropped from the macOS
  matrix, `smoke` and `e2e` macOS legs gated to push-to-main only,
  Dependabot grouped into one non-major PR + per-major PRs.

### Tests

- **Deep-health invariant guards (#57, #87).** Three new Rust
  integration tests lock in invariants that #56 deferred:
  `probe_pending_no_leak.rs` (4 tests — `ShimProcess::pending` does not
  accumulate across timeout / closed-channel / crashed-flag paths),
  `restart_count_lifecycle.rs` (5 tests — breaker-rejected calls do not
  bump `restart_count`, so `daemon.health({deep:true})` bookkeeping
  doesn't overcount), and `deep_health_probe.rs` (1 test, fake-chromium
  gated — full `ShimRequest::Health` → `ShimHealthInfo` round-trip).

## [0.9.2] — 2026-05-18

Linux-enablement patch release. With 0.9.1 a fresh Linux install would
`postinstall` correctly but then fail two ways: `loom doctor` reported
`chromium binary not found` (#70), and every `web.*` action surfaced as
`surface_trap` because the shim's IPC socket raced Tokio's I/O driver on
fd 3 (#71). 0.9.2 fixes both — `loom serve` + `loom action web.navigate`
now drives headless Chromium end-to-end on Linux without a system Chrome
on `$PATH`.

### Fixed

- **`loom doctor` + `loom session` Chromium detection on Linux/Windows
  (#70).** `loom doctor`'s `chromium_present_and_verified` check and the
  `chromium_resolver` launch path both hardcoded the macOS `.app` bundle
  layout (`Chromium.app/Contents/MacOS/Chromium`) regardless of host OS,
  while `loom postinstall` correctly extracted the pinned Chromium to the
  per-OS path (`chrome-linux/chrome` on Linux). Net effect on Linux:
  `loom doctor` reported `chromium binary not found` and the launcher
  could not find loom's own pinned download — it only worked when a
  system Chromium happened to be on `$PATH`. The per-OS layout is now a
  single shared source of truth,
  `loom_shared::chromium_resolver::chromium_binary_subpath()`, consulted by
  postinstall, doctor, and the resolver alike.

- **Shim IPC fd collision with Tokio I/O driver on Linux (#71).**
  `spawn_shim` `dup2`'d the IPC socketpair end onto fd 3, but
  `loom-shim-chromium`'s Tokio multi-thread runtime claims the lowest
  free descriptors at startup for its `epoll` instance + wakeup eventfd.
  Adopting fd 3 as the IPC socket then races the I/O driver:
  `UnixStream::from_std` returns `EINVAL`, the driver panics with
  `Bad file descriptor`, the shim exits, and every `web.*` action
  surfaces as `surface_trap`. The IPC socket is now pinned to fd 10 via
  a named `SHIM_IPC_FD` constant — well clear of the runtime's low-fd
  range. `loom-shims/tests/binary_smoke.rs` updated in lockstep.

## [0.9.1] — 2026-05-11

Patch release rolling up the post-0.9.0 daemon-stall fix (#55) and the
daemon-health deep-probe + SDK admin wrappers follow-up (#56), plus the
docs/CI groundwork from #51, #52, #54.

### Added

- **`daemon.health({deep: true})` shim probe.** Adds `ShimRequest::Health`
  CBOR variant + shim-side handler returning
  `ShimHealthInfo { uptime_ms, requests_served, last_request_at_ms }`. The
  daemon fans out probes concurrently with per-shim and overall budgets
  (defaults 1 s / 3 s; env-overridable via `LOOM_PROBE_TIMEOUT_MS` and
  `LOOM_DEEP_HEALTH_BUDGET_MS`). Payload is now typed
  `Vec<ShimDeepHealth>` (replacing the prior placeholder
  `Vec<serde_json::Value>`) with a typed
  `ProbeStatus { Ok, Timeout, Error }` enum. The new shim variant
  requires same-tree daemon+shim shipping; version-mismatched shims
  surface as `probe_status: "timeout"`. `daemon.health` continues to
  require the existing socket-auth token handshake — same auth surface
  as before; no anonymous reachability. Known gap: no per-connection
  rate limit on `daemon.health` itself (tracked as follow-up).
- **`ShimState.restart_count` / `last_restart_at_ms`.** Daemon-side
  bookkeeping for shim respawn events, bumped in `get_or_spawn` only
  when a prior state entry exists (avoids overcounting under
  open-breaker rejection). Exposed via the deep-health payload.
- **SDK wrappers (TS + Python) for the admin RPCs.** Hand-written
  `Session.kill()` (sync + async parity in Python), `killSession()`
  (TypeScript) / `kill_session()` (Python sync) admin free functions
  with documented "ADMIN ESCAPE HATCH — 5 s ceiling then SIGKILL"
  warnings, and `daemonHealth()` (TS) / `daemon_health()` (Python sync)
  free functions. JSON-RPC request `id` allocator switched from
  hardcoded `1` to monotonic per-connection; transports moved to
  id-keyed response demultiplexing (persistent reader task in async
  Python; persistent `data` listener in TS) to support `request.cancel`
  correlation while another call is in flight. Sync Python keeps
  single-in-flight and gains an `AsyncSession`-redirecting doc note for
  cancellation. TS adds `AbortSignal` support on `call()` via
  `LoomAbortError` (`name === "AbortError"`); Python async transport
  transparently emits `request.cancel` on `asyncio.CancelledError` and
  re-raises so `asyncio.wait_for` / `TaskGroup` / `asyncio.timeout`
  compose cleanly.
- **`SessionScope` primitive** ([`loom-core/src/session_scope/`](loom-core/src/session_scope/)).
  Per-session structured-concurrency parent: `tokio_util::sync::CancellationToken`
  paired with a `tokio::task::JoinSet`. All session-lifetime spawns become children;
  `drain(grace)` cancels cooperatively then force-aborts survivors. Replaces five
  fire-and-forget `tokio::spawn` sites that previously leaked `JoinHandle`s and
  saturated the daemon runtime after 4–6 sequential client sessions.
- **Per-request server-side deadline.** `LOOM_REQUEST_TIMEOUT_MS` (default
  `30000`) wraps `router.dispatch` so a hung shim or stuck dispatcher can't hold
  a connection task. Returns the typed `request-timeout` envelope on expiry.
- **`request.cancel` RPC.** Connection-scoped: cancels an in-flight request on
  the same connection by JSON-RPC `id`. Returns `{cancelled: bool}`. The
  cancelled request returns the typed `request-cancelled` envelope.
- **`session.kill` RPC.** Admin escape hatch for stuck sessions. Performs the
  abort flow plus blocks on shim teardown with a 5 s ceiling, then SIGKILL
  (`shutdown_process` already escalates SIGTERM(2s) → SIGKILL(1s) inside the
  ceiling).
- **`daemon.health` RPC.** Operational snapshot: `active_sessions`,
  `shim_breaker_states`, `otel_exporter` status. Shallow path is
  non-blocking; `{deep: true}` slot reserved for a follow-up shim probe.
- **`LoomErrorCode::RequestTimeout`** (wire string `request-timeout`) and
  **`LoomErrorCode::RequestCancelled`** (wire string `request-cancelled`).
  Both SemVer-minor additions per the existing wire-stability commitment.
- **Multi-session stress test** at
  [`loom-core/tests/multi_session_stall_repro.rs`](loom-core/tests/multi_session_stall_repro.rs)
  — 100 sequential session create+close cycles complete in under 1 s; the
  user-facing success criterion from the original investigation prompt.

### Changed

- **`ShimManager::record_failure`** circuit-breaker eviction no longer fires
  `tokio::spawn(shutdown_process(p))` fire-and-forget. Spawns into a
  `cleanup_tasks: JoinSet<()>` field with opportunistic `try_join_next`
  reaping on every `record_failure` and `shutdown_session`.
- **`CoreBridge::close_session_raw`** (daemon-side) similarly tracks
  `host.shutdown_session` background tasks in a `cleanup_tasks` JoinSet
  instead of leaking them via bare `tokio::spawn`.
- **`Session.budget_timer`** is now a SessionScope child with
  `tokio::select! { _ = cancel.cancelled() => {} _ = sleep(budget_ms) => ... }`
  so a closed session's timer exits cooperatively before its sleep
  completes, never tripping the kill callback after lifecycle exit.

### Removed

- **`Session.task_handle`** field — was dead code (declared but never
  populated). Replaced by the always-populated `Session.scope: Arc<SessionScope>`.
- **`Session.budget_timer`** field — superseded by the SessionScope-owned
  cancellation-aware timer.
- **`ReceiptMarshaller.queue_depth`** field + the docstring claiming
  `"Background queue depth 256; full → synchronous append on the calling task"`.
  The field was dead and the docstring lied — there was no bounded channel,
  no `try_send`, no backpressure. Removed both.

## [0.9.0] — 2026-05-05

Initial public pre-1.0 release. Loom incubated as an internal project before
this; see the README's [Status matrix](README.md#status) for which surfaces
are stable. Deterministic replay and `web.click` remain Beta — promotion to
1.0 requires those landing and the matrix CI green.

### Added

- **Determinism harness.** `Math.random()` (sfc32-seeded) and
  `Date.now()` / `performance.now()` (session-fixed epoch_ms) are pinned
  at session-create time and reproduced bit-for-bit on replay via the
  manifest's `started_at_ms`.
- **Implicit session management** in `loom-mcp serve`. MCP clients call
  `loom.web.navigate` with just `url` — the server lazily creates a
  session on first tool call and closes it on shutdown.
- **Path-traversal-safe session IDs.** Session paths reject anything
  that isn't a 26-char lowercase-alphanumeric ULID, preventing
  file-disclosure via `session.inspect '../evil'`.
- **Typed error mapping.** `kind: "http_status"` (with status code) for
  4xx/5xx responses, `kind: "wait_predicate_false"` for `web.wait`
  selector misses, `kind: "schema_violation"` for unsupported export
  formats with the actual supported list in the message.
- **Chromium subprocess crash detection.** The shim's supervisor spawns
  a task that `await`s `child.wait()`; the moment chromium dies, the
  shim self-exits → daemon's crash watcher fires → in-flight CDP
  commands fail in <1s instead of waiting the 30s recv timeout.
- **GC reference protection.** `collect_referenced_blobs` decodes
  `receipt_canonical_bytes` byte arrays and recursively scans for hex
  hashes, so `loom gc --ttl 0` no longer deletes blobs that active
  sessions still reference.
- **Stable session listing.** `loom session list` is now sorted by
  `created_at_ms` descending with `session_id` as tiebreak — independent
  of filesystem read_dir order.
- **Aborted-session replay refused.** Replaying an aborted or crashed
  source session now returns a typed `SessionAborted` error instead of
  silently producing a session that "replayed" an abandoned trace.
- **`import.playwright` RPC.** `loom import playwright trace.zip`
  imports a Playwright trace as a non-replayable Loom session.
- **Schema-aware CLI coercion.** `loom action web.type --text 5551234`
  no longer fails with `"5551234 is not of type 'string'"` — the parser
  consults the action's request schema and coerces per declared type.
- **Pretty TTY output.** Receipt-emitting subcommands auto-detect TTY
  and render colored multi-line layouts at a terminal; piped or
  redirected output stays canonical JSON, byte-for-byte identical to
  prior behaviour. New global flags: `--json`, `--pretty`, `--quiet` /
  `-q`, `--color={auto,always,never}` (with `--no-color` alias).
  Honours `NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR=0`, and `TERM=dumb`.
  Per-stream color resolution lets a piped stdout still ship colored
  stderr error prose.
- **Recursive sensitive-field redaction in pretty output.** Receipt
  fields whose key (at any nesting depth) matches a conservative
  denylist (`token`, `secret`, `password`, `api_key`, `oauth`,
  `credential`, `cookie`, `jwt`, `session_key`, `access_token`,
  `refresh_token`, `private_key`, `signing_key`, `client_secret`, etc.)
  render as `<redacted>` in the human path. The `--json` path is
  unchanged — machines still see the raw value.
- **Curated multi-line layouts** for every receipt-emitting subcommand:
  `session create / inspect / list / close / abort / replay / diff /
  validate / export`, `action <surface>.<verb>` (all `web.*` verbs),
  `vault add / list / grant / revoke`, `gc`, `doctor`,
  `import playwright`, `benchmark`. Each renderer declares the keys it
  consumed; remaining receipt fields surface in a dim "more details"
  tail block in JSON-Schema property order.
- **Empty-state copy.** `session list` with no sessions prints
  `No sessions found.` instead of an empty header table; `vault list`
  empty prints `No vault entries.`.
- **README "Status" matrix** declaring stable vs beta surfaces, with
  concrete promotion criteria for 1.0.
- **`loom --version`** prints `loom 0.9.0 (<short-sha> <build-date>)`
  so issue reports identify the exact build. The short SHA + UTC build
  date are baked in by `loom-cli/build.rs` from `git rev-parse` at
  compile time, with a `"unknown"` fallback for source-tarball builds
  (no `.git` directory). `SOURCE_DATE_EPOCH` is honored for
  reproducible builds. The JSON-emitted `VersionInfo` carries
  `build_date` alongside the existing `version`, `git_sha`, `target`.

### Changed — breaking

- **`--pretty` semantics changed.** Previously, `--pretty` produced
  indented JSON via `serde_json::to_string_pretty`. It now produces
  the human-readable colored multi-line layout described above. If
  your scripts depend on the old indented-JSON shape, switch to
  `--json` (canonical single-line, machine-parseable) — that's what
  you want for `jq` and similar pipelines anyway.
- **`NO_COLOR` empty-string behaviour fixed.** Previously, setting
  `NO_COLOR=""` (empty) was treated as a disable signal, contrary to
  the [no-color.org](https://no-color.org/) spec. The variable now
  disables color only when set to a non-empty value. CI environments
  that set `NO_COLOR=""` for parent processes will no longer
  accidentally disable color.
- **Workspace version walked back from 1.0.0 → 0.9.0** to reflect that
  deterministic replay and `web.click` are not yet bulletproof.
  `v0.9.0` is the first supported release.

### Fixed

- `web.wait` was a no-op — it dispatched a single `Runtime.evaluate`
  predicate but never checked the result, so missing selectors
  returned success in 1 tick. Now surfaces `wait_predicate_false`
  with `selector did not appear before timeout`.
- HTTP 4xx/5xx responses with `ERR_HTTP_RESPONSE_CODE_FAILURE` were
  classified as generic `network_error` losing the status code. The
  host's typed-error builder now prefers `status >= 400` events
  before `error_reason` events.
- `--budget network=0` and `wall_clock=0s` accepted silently (0
  internally meant "unlimited"). Now rejected at parse time.
- Profile help text claimed `safe`, `default` were valid profiles; the
  daemon's allowlist is `safe`, `standard`, `full`. Help text fixed.
- `Date.now()` / `Math.random()` leaked real wall-clock + unseeded
  values when called via `web.evaluate` *before* any `web.navigate`.
  Root cause: chromium's bootstrap `about:blank` context never had the
  determinism script applied. Fixed by lazy-spawning the
  determinism-injected target on the first evaluate.

[Unreleased]: https://github.com/mentiora-ai/loom/compare/v0.9.4...HEAD
[0.9.4]: https://github.com/mentiora-ai/loom/releases/tag/v0.9.4
[0.9.3]: https://github.com/mentiora-ai/loom/releases/tag/v0.9.3
[0.9.2]: https://github.com/mentiora-ai/loom/releases/tag/v0.9.2
[0.9.1]: https://github.com/mentiora-ai/loom/releases/tag/v0.9.1
[0.9.0]: https://github.com/mentiora-ai/loom/releases/tag/v0.9.0
