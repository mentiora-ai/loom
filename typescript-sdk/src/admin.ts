/**
 * Daemon-level calls that need no session: listing, vault grants, kill, health.
 */
import { LoomTransport } from "./transport.js";
import type {
  SessionInfo,
  GrantInfo,
  DaemonHealthResult,
  ProbeStatus,
} from "./types.js";
import { toSessionInfo } from "./wire.js";

export async function sessionList(opts: {
  socketPath?: string;
  token?: string;
} = {}): Promise<SessionInfo[]> {
  const transport = new LoomTransport(opts.socketPath, opts.token);
  await transport.connect();
  try {
    const result = (await transport.call("session.list", {})) as Array<Record<string, unknown>>;
    return (result ?? []).map(toSessionInfo);
  } finally {
    await transport.close();
  }
}

export async function vaultGrant(
  sessionId: string,
  origin: string,
  scopes: string[],
  ttlSeconds: number,
  label: string,
  opts: { socketPath?: string; token?: string } = {},
): Promise<GrantInfo> {
  const transport = new LoomTransport(opts.socketPath, opts.token);
  await transport.connect();
  try {
    const result = (await transport.call("vault.grant", {
      session_id: sessionId,
      origin,
      scopes,
      ttl_seconds: ttlSeconds,
      label,
    })) as Record<string, unknown>;
    return {
      grantId: result["grant_id"] as string,
      origin: (result["origin"] as string) ?? "",
      scopes: (result["scopes"] as string[]) ?? [],
      ttlSeconds: (result["ttl_seconds"] as number) ?? 0,
      label: (result["label"] as string) ?? "",
    };
  } finally {
    await transport.close();
  }
}

export async function vaultRevoke(
  grantId: string,
  reason: string,
  opts: { socketPath?: string; token?: string } = {},
): Promise<void> {
  const transport = new LoomTransport(opts.socketPath, opts.token);
  await transport.connect();
  try {
    await transport.call("vault.revoke", { grant_id: grantId, reason });
  } finally {
    await transport.close();
  }
}

export async function vaultListGrants(
  sessionId?: string,
  opts: { socketPath?: string; token?: string } = {},
): Promise<GrantInfo[]> {
  const transport = new LoomTransport(opts.socketPath, opts.token);
  await transport.connect();
  try {
    const params: Record<string, unknown> = {};
    if (sessionId !== undefined) params["session_id"] = sessionId;
    const result = (await transport.call("vault.list_grants", params)) as Array<
      Record<string, unknown>
    >;
    return (result ?? []).map((g) => ({
      grantId: g["grant_id"] as string,
      origin: (g["origin"] as string) ?? "",
      scopes: (g["scopes"] as string[]) ?? [],
      ttlSeconds: (g["ttl_seconds"] as number) ?? 0,
      label: (g["label"] as string) ?? "",
    }));
  } finally {
    await transport.close();
  }
}

// ─── admin RPCs (session.kill, daemon.health) ────────────────────────────

/** Internal: single `session.kill` call site shared by Session.kill() and
 *  the top-level killSession() free function. */
export async function _doKillSession(transport: LoomTransport, sessionId: string): Promise<void> {
  await transport.call("session.kill", { session_id: sessionId });
}

/**
 * ADMIN ESCAPE HATCH — force-terminate a stuck session by id without
 * holding a Session handle. Performs the abort flow plus a blocking 5s
 * shim-teardown ceiling, then SIGKILL. Prefer `session.close()` for normal
 * shutdown; reach for `killSession()` only when normal shutdown is wedged.
 *
 * The daemon authenticates the calling transport at the connection level
 * (HELLO token handshake) — there is no separate per-call auth on this
 * admin function.
 */
export async function killSession(
  sessionId: string,
  opts: { socketPath?: string; token?: string } = {},
): Promise<void> {
  const transport = new LoomTransport(opts.socketPath, opts.token);
  await transport.connect();
  try {
    await _doKillSession(transport, sessionId);
  } finally {
    await transport.close();
  }
}

/**
 * Query daemon health. Shallow path is non-blocking. `{deep: true}` fans
 * out a per-shim probe (1s budget per shim, 3s overall) and returns
 * uptime/requests-served counters per running shim.
 *
 * The optional `signal` cancels the in-flight call: the transport fires a
 * `request.cancel` envelope and rejects with `LoomAbortError`.
 *
 * Auth: requires the existing socket-auth token; no separate per-call gate.
 */
export async function daemonHealth(
  opts: {
    deep?: boolean;
    signal?: AbortSignal;
    socketPath?: string;
    token?: string;
  } = {},
): Promise<DaemonHealthResult> {
  const transport = new LoomTransport(opts.socketPath, opts.token);
  await transport.connect();
  try {
    const result = (await transport.call(
      "daemon.health",
      { deep: opts.deep ?? false },
      { signal: opts.signal },
    )) as Record<string, unknown>;
    return toDaemonHealthResult(result);
  } finally {
    await transport.close();
  }
}

function toDaemonHealthResult(d: Record<string, unknown>): DaemonHealthResult {
  return {
    activeSessions: (d["active_sessions"] as number) ?? 0,
    shimBreakerStates: ((d["shim_breaker_states"] as Array<Record<string, unknown>>) ?? []).map(
      (s) => ({
        shimId: s["shim_id"] as string,
        state: s["state"] as string,
        consecutiveFailures: (s["consecutive_failures"] as number) ?? 0,
        openedAtMs: (s["opened_at_ms"] as number | null) ?? null,
      }),
    ),
    otelExporter: (d["otel_exporter"] as string) ?? "unknown",
    orphanBrowserTrees: (d["orphan_browser_trees"] as number) ?? 0,
    oldestActiveSessionAgeSecs: (d["oldest_active_session_age_secs"] as number | null) ?? null,
    deep:
      d["deep"] === null || d["deep"] === undefined
        ? null
        : (d["deep"] as Array<Record<string, unknown>>).map((s) => ({
            shimId: s["shim_id"] as string,
            daemonRestartCount: (s["daemon_restart_count"] as number) ?? 0,
            daemonLastRestartAtMs: (s["daemon_last_restart_at_ms"] as number | null) ?? null,
            shimUptimeMs: (s["shim_uptime_ms"] as number) ?? 0,
            shimRequestsServed: (s["shim_requests_served"] as number) ?? 0,
            shimLastRequestAtMs: (s["shim_last_request_at_ms"] as number | null) ?? null,
            probeStatus: s["probe_status"] as ProbeStatus,
          })),
  };
}
