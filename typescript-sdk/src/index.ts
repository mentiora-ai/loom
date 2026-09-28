/**
 * @mentiora-ai/loom-sdk — TypeScript client library for the loom browser-automation daemon.
 *
 * Quick start:
 *
 *   import { Session } from "@mentiora-ai/loom-sdk";
 *
 *   const session = await Session.create();
 *   await session.navigate("https://example.com");
 *   await session.close();
 *
 *   // Or with async disposal:
 *   await using session = await Session.create();
 *   await session.navigate("https://example.com");
 */

export { Session } from "./session.js";
export {
  sessionList,
  vaultGrant,
  vaultRevoke,
  vaultListGrants,
  killSession,
  daemonHealth,
} from "./admin.js";
export { LoomTransport } from "./transport.js";
export { LOOM_ERROR_CODES } from "./types.js";
export {
  LoomError,
  LoomRPCError,
  LoomConnectionError,
  LoomTokenError,
  LoomAbortError,
} from "./errors.js";
export type {
  SessionInfo,
  SessionInspection,
  Receipt,
  ReceiptError,
  DiffReport,
  ExportInfo,
  ValidationResult,
  GrantInfo,
  MethodSchema,
  SchemaRegistry,
  LoomErrorCode,
  ProbeStatus,
  ShimBreakerSnapshot,
  ShimDeepHealth,
  DaemonHealthResult,
} from "./types.js";
