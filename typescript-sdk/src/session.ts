/**
 * Session — high-level loom client.
 *
 * Create via `await Session.create()`. Use `await using` or call `close()` explicitly.
 */
import { LoomTransport } from "./transport.js";
import { LoomError } from "./errors.js";
import type {
  SessionInfo,
  SessionInspection,
  DiffReport,
  ExportInfo,
  ValidationResult,
  Receipt,
  SchemaRegistry,
  JsonSchemaObject,
} from "./types.js";
import { _doKillSession } from "./admin.js";
import { buildActionParams, hexToBytes, toSessionInfo, toReceipt } from "./wire.js";

export interface SessionCreateOptions {
  profile?: string;
  /**
   * Page-network mode. `"live"` (the default) is the ONLY valid value:
   * page traffic is always fetched live from the network — loom does not
   * record or replay page-network responses, and response bodies are never
   * captured (HAR exports carry no bodies). Any other value (including the
   * formerly-accepted-but-inert `"recorded"`/`"mixed"`) is rejected by the
   * daemon with `invalid_network_mode`.
   */
  networkMode?: string;
  capture?: boolean;
  seed?: number;
  budget?: unknown;
  socketPath?: string;
  token?: string;
  /**
   * Fixed Unix epoch in milliseconds that pins the injected browser clock
   * (cross-run determinism). Two recordings created with the same `seed` +
   * `clockAnchor` capture identical dom/screenshot/outcome hashes, so
   * `session.diff` between them reports zero field diffs. Sent on the wire
   * as `clock_anchor`; omitted (default) → the session epoch falls back to
   * wall-clock now. No effect under `noDeterminism`.
   */
  clockAnchor?: number;
  /**
   * Disable determinism for this session (settle-capture). Determinism is ON
   * by default: loom freezes `Date.now`/animations and seeds `Math.random` so
   * captures are byte-reproducible. Set `true` for live/non-reproducible
   * capture (real clock + unseeded RNG). Such a session is recorded as
   * NON-REPLAYABLE — replay refuses it.
   */
  noDeterminism?: boolean;
}

export class Session {
  readonly sessionId: string;
  readonly status: string;
  private readonly _transport: LoomTransport;
  // Latched result of the first close(). Disposable resources must
  // tolerate double-dispose: `await using` + an explicit close() inside
  // the block runs close() twice (the second via Symbol.asyncDispose).
  private _closeInfo: SessionInfo | null = null;

  private constructor(sessionId: string, status: string, transport: LoomTransport) {
    this.sessionId = sessionId;
    this.status = status;
    this._transport = transport;
  }

  static async create(opts: SessionCreateOptions = {}): Promise<Session> {
    const transport = new LoomTransport(opts.socketPath, opts.token);
    await transport.connect();
    try {
      const params: Record<string, unknown> = {
        profile: opts.profile ?? "default",
        network_mode: opts.networkMode ?? "live",
        capture: opts.capture ?? true,
      };
      if (opts.seed !== undefined) params["seed"] = opts.seed;
      if (opts.clockAnchor !== undefined) params["clock_anchor"] = opts.clockAnchor;
      if (opts.budget !== undefined) params["budget"] = opts.budget;
      if (opts.noDeterminism) params["no_determinism"] = true;
      const result = (await transport.call("session.create", params)) as Record<string, unknown>;
      return new Session(
        result["session_id"] as string,
        (result["status"] as string) ?? "active",
        transport,
      );
    } catch (err) {
      // Don't leak the connected socket when the RPC fails (schema
      // violation, unknown profile, auth failure, …) — an open net.Socket
      // is an active libuv handle that keeps the process alive.
      await transport.close();
      throw err;
    }
  }

  /**
   * Close the session and release the socket. Idempotent: a second
   * close() (explicit or via `await using` disposal) is a no-op that
   * returns the latched SessionInfo instead of re-issuing the RPC on
   * the already-destroyed transport.
   */
  async close(): Promise<SessionInfo> {
    if (this._closeInfo) return this._closeInfo;
    let result: Record<string, unknown> | null = null;
    try {
      result = (await this._transport.call("session.close", {
        session_id: this.sessionId,
      })) as Record<string, unknown> | null;
    } finally {
      // Latch even when the RPC fails: the transport is destroyed below
      // either way, so a retry can never succeed — the first call's
      // error (if any) still propagates, but a later double-dispose
      // must not throw at scope exit.
      this._closeInfo = result
        ? toSessionInfo(result)
        : { sessionId: this.sessionId, status: "closed", createdAtMs: 0 };
      // Always release the socket, even when the RPC fails.
      await this._transport.close();
    }
    return this._closeInfo;
  }

  async abort(reason: string): Promise<SessionInfo> {
    const result = (await this._transport.call("session.abort", {
      session_id: this.sessionId,
      reason,
    })) as Record<string, unknown>;
    return toSessionInfo(result);
  }

  /**
   * Force-terminate a stuck session.
   *
   * Use `close()` for normal shutdown. Use `abort()` to cancel in-flight
   * actions while keeping the session. Use `kill()` ONLY when those
   * don't return — it tears down the shim with a 5s ceiling then SIGKILL.
   *
   * Delegates to {@link _doKillSession} to keep one RPC call site.
   */
  async kill(): Promise<void> {
    return _doKillSession(this._transport, this.sessionId);
  }

  async inspect(atAction?: number): Promise<SessionInspection> {
    const params: Record<string, unknown> = { session_id: this.sessionId };
    if (atAction !== undefined) params["at_action"] = atAction;
    const result = (await this._transport.call("session.inspect", params)) as Record<
      string,
      unknown
    >;
    return {
      sessionId: result["session_id"] as string,
      atAction: (result["at_action"] as number | null) ?? null,
      manifestSummary: (result["manifest_summary"] as Record<string, unknown>) ?? {},
    };
  }

  async export(format: string): Promise<ExportInfo> {
    const result = (await this._transport.call("session.export", {
      session_id: this.sessionId,
      format,
    })) as Record<string, unknown>;
    return {
      sessionId: result["session_id"] as string,
      format: result["format"] as string,
      artifactRef: (result["artifact_ref"] as string) ?? "",
    };
  }

  async validate(): Promise<ValidationResult> {
    const result = (await this._transport.call("session.validate", {
      session_id: this.sessionId,
    })) as Record<string, unknown>;
    return {
      sessionId: result["session_id"] as string,
      passed: (result["passed"] as boolean) ?? false,
      reasons: (result["reasons"] as string[]) ?? [],
      replayable: (result["replayable"] as boolean) ?? true,
      notReplayableReason: result["not_replayable_reason"] as string | undefined,
    };
  }

  // No networkMode option here: earlier SDKs sent an inert
  // `network_mode: "replay"` the daemon ignored — replay re-executes from
  // the recorded manifest and has no page-network mode to choose.
  async replay(opts: { speed?: number } = {}): Promise<SessionInfo> {
    const result = (await this._transport.call("session.replay", {
      session_id: this.sessionId,
      speed: opts.speed ?? 1.0,
    })) as Record<string, unknown>;
    return toSessionInfo(result);
  }

  async diff(
    otherSessionId: string,
    opts: { includeScreenshots?: boolean; showDomDiffs?: boolean } = {},
  ): Promise<DiffReport> {
    const result = (await this._transport.call("session.diff", {
      a: this.sessionId,
      b: otherSessionId,
      include_screenshots: opts.includeScreenshots ?? false,
      show_dom_diffs: opts.showDomDiffs ?? false,
    })) as Record<string, unknown>;
    return {
      a: result["a"] as string,
      b: result["b"] as string,
      diff: (result["diff"] as Record<string, unknown>) ?? {},
    };
  }

  /**
   * Navigate the page and capture DOM + screenshot, gating the capture on a
   * readiness state (settle-capture). By default loom waits until the page is
   * `"settled"` (network quiet + `readyState` complete + the final URL stable
   * after client-side redirects + the DOM quiescent) so the capture is a real
   * rendered page, not a blank SPA shell or an arbitrary animation frame.
   *
   * - `until`: `"load"` | `"networkidle"` | `"settled"` (default `"settled"`).
   * - `timeoutMs`: bound on the readiness wait. If readiness is never reached
   *   (persistent connection, perpetual animation) the call still returns — the
   *   receipt's {@link Receipt.settleOutcome} is `"timeout"` / `"dom_unstable"`
   *   instead of `"reached"`. It never hangs.
   */
  async navigate(
    url: string,
    opts: { deadlineMs?: number; until?: "load" | "networkidle" | "settled"; timeoutMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = { url };
    if (opts.until !== undefined) payload["until"] = opts.until;
    if (opts.timeoutMs !== undefined) payload["timeout_ms"] = opts.timeoutMs;
    const result = (await this._transport.call(
      "action.web.navigate",
      buildActionParams(this.sessionId, "navigate", payload, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Wait for the CURRENT page to reach a readiness state (settle-capture),
   * without navigating. Use after a navigate or an interaction that triggers
   * async re-render to gate a subsequent screenshot/snapshot on real
   * readiness instead of a magic sleep.
   *
   * - `until`: `"load"` | `"networkidle"` | `"settled"` (default `"settled"`).
   * - `timeoutMs`: bound on the wait. If readiness is never reached the call
   *   still returns — the receipt's {@link Receipt.settleOutcome} is
   *   `"timeout"` / `"dom_unstable"` instead of `"reached"`. It never hangs.
   */
  async waitFor(
    opts: { deadlineMs?: number; until?: "load" | "networkidle" | "settled"; timeoutMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = {};
    if (opts.until !== undefined) payload["until"] = opts.until;
    if (opts.timeoutMs !== undefined) payload["timeout_ms"] = opts.timeoutMs;
    const result = (await this._transport.call(
      "action.web.wait_for",
      buildActionParams(this.sessionId, "wait_for", payload, opts.deadlineMs ?? 30000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async click(selector: string, opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.click",
      buildActionParams(this.sessionId, "click", { selector }, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Type `text` into `selector`.
   *
   * `mode: "fill"` (default) focuses the element, selects its existing content,
   * and commits `text` via a single CDP `Input.insertText` — a genuine
   * (`isTrusted:true`) edit, the same mechanism as Playwright `fill()`. It drives
   * React/react-hook-form `onChange` AND is treated as user-entered, so
   * trust-gating flows (e.g. Auth0 New Universal Login) advance; `text: ""`
   * clears the field. Date/time-family inputs (`date`, `time`,
   * `datetime-local`, `month`, `week`, `color`, `range`) are set by value in their
   * own format (`2036-12-31` for a date); a value the input rejects fails with
   * `malformed_value`, and a disabled/readonly target with `not_editable`.
   * `mode: "value"` is the legacy path: `.value` via
   * `Runtime.evaluate` + synthetic `input`/`change` events (`isTrusted:false`) —
   * a back-compat escape hatch. `mode: "keystrokes"` dispatches a real
   * per-character CDP `Input.dispatchKeyEvent` sequence (`isTrusted:true`).
   */
  async typeText(
    selector: string,
    text: string,
    opts: { mode?: "fill" | "value" | "keystrokes"; deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = { selector, text };
    if (opts.mode !== undefined) payload["mode"] = opts.mode;
    const result = (await this._transport.call(
      "action.web.type_text",
      buildActionParams(this.sessionId, "type_text", payload, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Dispatch a real key press (`isTrusted:true`) via CDP
   * `Input.dispatchKeyEvent`. `key` is a named key (`Enter`, `Tab`, `Escape`,
   * arrows, …) or a single printable character; `modifiers` may include
   * `Control`, `Alt`, `Shift`, `Meta`. With `selector` the element is focused
   * first; otherwise the event targets whatever currently has focus.
   */
  async pressKey(
    key: string,
    opts: { selector?: string; modifiers?: string[]; deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = { key };
    if (opts.selector !== undefined) payload["selector"] = opts.selector;
    if (opts.modifiers !== undefined) payload["modifiers"] = opts.modifiers;
    const result = (await this._transport.call(
      "action.web.press_key",
      buildActionParams(this.sessionId, "press_key", payload, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async select(
    selector: string,
    value: string,
    opts: { deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.select",
      buildActionParams(this.sessionId, "select", { selector, value }, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async hover(selector: string, opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.hover",
      buildActionParams(this.sessionId, "hover", { selector }, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async scroll(
    selector: string,
    opts: { deltaY?: number; deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.scroll",
      buildActionParams(
        this.sessionId,
        "scroll",
        { selector, delta_y: opts.deltaY ?? 300 },
        opts.deadlineMs ?? 5000,
      ),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async wait(selector: string, opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.wait",
      buildActionParams(this.sessionId, "wait", { selector }, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async evaluate(expression: string, opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.evaluate",
      buildActionParams(
        this.sessionId,
        "evaluate",
        { expression },
        opts.deadlineMs ?? 5000,
      ),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async screenshot(opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.screenshot",
      buildActionParams(this.sessionId, "screenshot", {}, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Start recording a video (screencast) of the page. Bracket a sequence of
   * actions between `startRecording()` and `stopRecording()`; the latter returns
   * the `.webm` content hash. Caps (all optional, safe defaults) auto-stop the
   * recording: `maxDurationMs` (300000), `maxBytes` (268435456), `frameRate` (10).
   *
   * NOTE: a recording captures whatever is on screen — including any passwords
   * or PII rendered during the window (same posture as `screenshot()`).
   */
  async startRecording(
    opts: { maxDurationMs?: number; maxBytes?: number; frameRate?: number; deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = {};
    if (opts.maxDurationMs !== undefined) payload["max_duration_ms"] = opts.maxDurationMs;
    if (opts.maxBytes !== undefined) payload["max_bytes"] = opts.maxBytes;
    if (opts.frameRate !== undefined) payload["frame_rate"] = opts.frameRate;
    const result = (await this._transport.call(
      "action.web.start_recording",
      buildActionParams(this.sessionId, "start_recording", payload, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Stop the active recording, encode it to `.webm`, and return a Receipt whose
   * `screencastAfterHash` points at the video in CAS. The default deadline is
   * generous because the encode runs synchronously. A best-effort encode failure
   * returns an error receipt (the session is unaffected).
   */
  async stopRecording(opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.stop_recording",
      buildActionParams(this.sessionId, "stop_recording", {}, opts.deadlineMs ?? 120000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Fetch the recorded `.webm` referenced by a `stopRecording()` receipt and
   * return its bytes. Throws if the receipt carries no `screencastAfterHash`
   * (e.g. the encode failed).
   */
  async fetchRecording(receipt: Receipt): Promise<Uint8Array> {
    if (!receipt.screencastAfterHash) {
      throw new Error("receipt has no screencastAfterHash (recording failed?)");
    }
    const content = (await this._transport.call("content.get", {
      artifact_ref: receipt.screencastAfterHash,
    })) as Record<string, unknown>;
    return hexToBytes(content["data_hex"]);
  }

  /**
   * Inject caller-provided audio into the session's synthetic microphone
   * (`--audio` sessions only). Provide the payload as `blobRef` (a CAS
   * hash, resolved daemon-side — preferred) or `audioB64` (inline base64
   * for short clips). Resolves when the buffer is enqueued; set
   * `awaitPlayout` to resolve when playout completes.
   */
  async injectAudio(
    opts: { blobRef?: string; audioB64?: string; awaitPlayout?: boolean; deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = {};
    if (opts.blobRef !== undefined) payload["blob_ref"] = opts.blobRef;
    if (opts.audioB64 !== undefined) payload["audio_b64"] = opts.audioB64;
    if (opts.awaitPlayout !== undefined) payload["await_playout"] = opts.awaitPlayout;
    const result = (await this._transport.call(
      "action.web.inject_audio",
      buildActionParams(this.sessionId, "inject_audio", payload, opts.deadlineMs ?? 30000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Start capturing the session's inbound WebRTC audio. Caps (optional,
   * safe defaults) truncate rather than error: `maxDurationMs`, `maxBytes`.
   */
  async startAudioCapture(
    opts: { maxDurationMs?: number; maxBytes?: number; deadlineMs?: number } = {},
  ): Promise<Receipt> {
    const payload: Record<string, unknown> = {};
    if (opts.maxDurationMs !== undefined) payload["max_duration_ms"] = opts.maxDurationMs;
    if (opts.maxBytes !== undefined) payload["max_bytes"] = opts.maxBytes;
    const result = (await this._transport.call(
      "action.web.start_audio_capture",
      buildActionParams(this.sessionId, "start_audio_capture", payload, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  /**
   * Stop the active audio capture and return a Receipt whose
   * `audioAfterHash` points at the 16 kHz mono WAV in CAS. When
   * `audioStopReason` is `byte_cap`/`duration_cap` the capture was
   * truncated at a cap and a warning is emitted on stderr — silent
   * truncation is a trust failure.
   */
  async stopAudioCapture(opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.stop_audio_capture",
      buildActionParams(this.sessionId, "stop_audio_capture", {}, opts.deadlineMs ?? 30000),
    )) as Record<string, unknown>;
    const receipt = toReceipt(result);
    if (receipt.audioStopReason === "byte_cap" || receipt.audioStopReason === "duration_cap") {
      console.warn(
        `loom: audio capture truncated at ${receipt.audioStopReason} — raise maxBytes/maxDurationMs on startAudioCapture to keep more`,
      );
    }
    return receipt;
  }

  /**
   * Fetch the captured WAV referenced by a `stopAudioCapture()` receipt
   * and return its bytes. Throws if the receipt carries no
   * `audioAfterHash` (capture errored or nothing was captured).
   */
  async fetchAudioCapture(receipt: Receipt): Promise<Uint8Array> {
    if (!receipt.audioAfterHash) {
      throw new Error("receipt has no audioAfterHash — did stopAudioCapture succeed?");
    }
    const content = (await this._transport.call("content.get", {
      artifact_ref: receipt.audioAfterHash,
    })) as Record<string, unknown>;
    return hexToBytes(content["data_hex"]);
  }

  /**
   * Fetch the captured WAV referenced by a `stopAudioCapture()` receipt
   * and write it to `path` as a playable file (overwrites an existing
   * file).
   */
  async saveAudioCapture(receipt: Receipt, path: string): Promise<void> {
    const bytes = await this.fetchAudioCapture(receipt);
    const fs = await import("node:fs/promises");
    await fs.writeFile(path, bytes);
  }

  async snapshot(opts: { deadlineMs?: number } = {}): Promise<Receipt> {
    const result = (await this._transport.call(
      "action.web.snapshot",
      buildActionParams(this.sessionId, "snapshot", {}, opts.deadlineMs ?? 5000),
    )) as Record<string, unknown>;
    return toReceipt(result);
  }

  async schemas(): Promise<SchemaRegistry> {
    const result = (await this._transport.call("rpc.schemas", {})) as Record<
      string,
      unknown
    > | null;
    // A version-skewed or misbehaving daemon may omit `methods`; surface a
    // typed error instead of a raw TypeError from `.map` of undefined,
    // keeping the SDK's uniform LoomError surface.
    const methods = result?.["methods"];
    if (!Array.isArray(methods)) {
      throw new LoomError(
        "malformed rpc.schemas response: missing 'methods' array (daemon version skew?)",
      );
    }
    return {
      methods: (methods as Array<Record<string, unknown>>).map((m) => ({
        method: m["method"] as string,
        request: (m["request"] as JsonSchemaObject) ?? {},
        response: (m["response"] as JsonSchemaObject) ?? {},
      })),
      sourceWitSha256: (result?.["source_wit_sha256"] as string) ?? "",
    };
  }

  [Symbol.asyncDispose](): Promise<SessionInfo> {
    return this.close();
  }
}
