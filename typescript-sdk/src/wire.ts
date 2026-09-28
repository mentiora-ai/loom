/**
 * Wire (de)serialisation shared by Session and the admin calls: action params
 * out, snake_case daemon records in.
 */
import type { SessionInfo, Receipt, ReceiptError } from "./types.js";

export function buildActionParams(
  sessionId: string,
  kind: string,
  payload: Record<string, unknown>,
  deadlineMs: number,
): Record<string, unknown> {
  return {
    session_id: sessionId,
    action: {
      kind,
      payload: Array.from(Buffer.from(JSON.stringify(payload), "utf8")),
      deadline_ms: deadlineMs,
    },
  };
}

export function hexToBytes(hex: unknown): Uint8Array {
  // The daemon always hex-encodes content.get responses; a missing field or
  // malformed hex means a broken/foreign endpoint — throw a typed error
  // rather than a cryptic TypeError or silently zero-filled bytes.
  if (typeof hex !== "string") {
    throw new Error("content.get response missing data_hex");
  }
  if (hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) {
    throw new Error("malformed data_hex in content.get response");
  }
  // Native decode — a per-byte parseInt loop blocks the event loop on
  // multi-MB blobs. Validity is guaranteed by the regex above (Buffer.from
  // truncates silently on bad input, so the guard is load-bearing).
  return Buffer.from(hex, "hex");
}

export function toSessionInfo(d: Record<string, unknown>): SessionInfo {
  return {
    sessionId: d["session_id"] as string,
    status: d["status"] as string,
    createdAtMs: (d["created_at_ms"] as number) ?? 0,
  };
}

export function toReceiptError(raw: unknown): ReceiptError | undefined {
  // The daemon serializes `error: null` on success — treat null/non-object
  // the same as absent.
  if (raw === null || typeof raw !== "object") return undefined;
  const e = raw as Record<string, unknown>;
  return { kind: (e["kind"] as string) ?? "", detail: e["detail"] };
}

export function toReceipt(d: Record<string, unknown>): Receipt {
  // Receipt-level outcome ("success" | "error" | "aborted"). Failed actions
  // return as a SUCCESSFUL JSON-RPC result whose receipt has status="error"
  // — surface it so callers can distinguish failures from successes.
  const status = (d["status"] as string) ?? "success";
  return {
    actionHash: (d["action_hash"] as string) ?? "",
    outcomeHash: (d["outcome_hash"] as string) ?? "",
    emittedAtMs: (d["emitted_at_ms"] as number) ?? 0,
    status,
    ok: status === "success",
    error: toReceiptError(d["error"]),
    // navigate tier-2 fields: absent (→ undefined) for non-navigate verbs.
    url: d["url"] as string | undefined,
    finalUrl: d["final_url"] as string | undefined,
    title: d["title"] as string | undefined,
    statusCode: d["status_code"] as number | undefined,
    domSnapshotHash: d["dom_snapshot_hash"] as string | undefined,
    screenshotAfterHash: d["screenshot_after_hash"] as string | undefined,
    screencastAfterHash: d["screencast_after_hash"] as string | undefined,
    audioAfterHash: d["audio_after_hash"] as string | undefined,
    audioStopReason: d["audio_stop_reason"] as string | undefined,
    // evaluate tier fields.
    returnValueJson: d["return_value_json"] as string | undefined,
    returnValueBlobRef: d["return_value_blob_ref"] as string | undefined,
    // settle-capture: present on navigate receipts; absent (→ undefined) on
    // verbs without a readiness gate.
    settleUntil: d["settle_until"] as string | undefined,
    settleOutcome: d["settle_outcome"] as string | undefined,
  };
}
