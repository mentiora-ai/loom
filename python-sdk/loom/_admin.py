"""Daemon-level calls that need no session: listing, vault grants, kill, health."""

from __future__ import annotations

from typing import Any

from loom._transport import LoomTransport
from loom.types import (
    GrantInfo,
    SessionInfo,
)


def session_list(*, socket_path: str | None = None, token: str | None = None) -> list[SessionInfo]:
    """List all sessions on the daemon."""
    with LoomTransport(socket_path, token) as t:
        result = t.call("session.list", {})
        return [SessionInfo._from_dict(s) for s in (result or [])]


def vault_grant(
    session_id: str,
    origin: str,
    scopes: list[str],
    ttl_seconds: int,
    label: str,
    *,
    socket_path: str | None = None,
    token: str | None = None,
) -> GrantInfo:
    with LoomTransport(socket_path, token) as t:
        result = t.call(
            "vault.grant",
            {
                "session_id": session_id,
                "origin": origin,
                "scopes": scopes,
                "ttl_seconds": ttl_seconds,
                "label": label,
            },
        )
        return GrantInfo._from_dict(result)


def vault_revoke(
    grant_id: str,
    reason: str,
    *,
    socket_path: str | None = None,
    token: str | None = None,
) -> None:
    with LoomTransport(socket_path, token) as t:
        t.call("vault.revoke", {"grant_id": grant_id, "reason": reason})


def vault_list_grants(
    session_id: str | None = None,
    *,
    socket_path: str | None = None,
    token: str | None = None,
) -> list[GrantInfo]:
    params: dict[str, Any] = {}
    if session_id is not None:
        params["session_id"] = session_id
    with LoomTransport(socket_path, token) as t:
        result = t.call("vault.list_grants", params)
        return [GrantInfo._from_dict(g) for g in (result or [])]


# ─── admin RPCs (kill_session, daemon_health) ─────────────────────────────


def _do_kill_session_sync(transport: LoomTransport, session_id: str) -> None:
    """Internal single call site shared by ``Session.kill()`` and
    the top-level ``kill_session()`` free function."""
    transport.call("session.kill", {"session_id": session_id})


def kill_session(
    session_id: str,
    *,
    socket_path: str | None = None,
    token: str | None = None,
) -> None:
    """ADMIN ESCAPE HATCH — force-terminate a stuck session by id without
    holding a :class:`Session` handle.

    Performs the abort flow plus a blocking 5 s shim-teardown ceiling,
    then SIGKILL. Prefer ``Session.close()`` for normal shutdown; reach
    for ``kill_session()`` only when normal shutdown is wedged.

    The daemon authenticates the calling transport at the connection
    level (HELLO token handshake) — there is no separate per-call gate
    on this admin function.
    """
    with LoomTransport(socket_path, token) as t:
        _do_kill_session_sync(t, session_id)


def daemon_health(
    *,
    deep: bool = False,
    socket_path: str | None = None,
    token: str | None = None,
) -> dict[str, Any]:
    """Query daemon health.

    Shallow path is non-blocking. ``deep=True`` fans out a per-shim probe
    (1 s budget per shim, 3 s overall) and returns uptime/requests-served
    counters per running shim.

    Returns the parsed JSON payload as a dict; field names use snake_case
    (matching the wire format) — see ``loom-rpc/src/rpc_handlers/rpc_handlers.rs``
    for the field schema.
    """
    with LoomTransport(socket_path, token) as t:
        return t.call("daemon.health", {"deep": deep})
