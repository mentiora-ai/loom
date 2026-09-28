"""The asynchronous :class:`AsyncSession`."""

from __future__ import annotations

from typing import Any

from loom._async_transport import AsyncLoomTransport
from loom._payloads import _build_action_params, _navigate_payload, _wait_for_payload
from loom.types import (
    Receipt,
    SessionInfo,
)


class AsyncSession:
    """
    Asyncio loom session handle.

    Create via ``await AsyncSession.create()``. Use as an async context
    manager or call ``await close()`` explicitly.
    """

    def __init__(self, session_id: str, status: str, transport: AsyncLoomTransport) -> None:
        self.session_id = session_id
        self.status = status
        self._transport = transport

    @classmethod
    async def create(
        cls,
        *,
        profile: str = "default",
        network_mode: str = "live",
        capture: bool = True,
        seed: int | None = None,
        clock_anchor: int | None = None,
        budget: Any = None,
        socket_path: str | None = None,
        token: str | None = None,
        no_determinism: bool = False,
    ) -> AsyncSession:
        transport = await AsyncLoomTransport.connect(socket_path, token)
        params: dict[str, Any] = {
            "profile": profile,
            "network_mode": network_mode,
            "capture": capture,
        }
        if seed is not None:
            params["seed"] = seed
        if clock_anchor is not None:
            params["clock_anchor"] = clock_anchor
        if budget is not None:
            params["budget"] = budget
        if no_determinism:
            params["no_determinism"] = True
        try:
            result = await transport.call("session.create", params)
            return cls(
                session_id=result["session_id"],
                status=result.get("status", "active"),
                transport=transport,
            )
        except Exception:
            # Don't leak the connected socket when the RPC fails (schema
            # violation, unknown profile, auth failure, …).
            await transport.close()
            raise

    async def navigate(
        self,
        url: str,
        *,
        deadline_ms: int = 5000,
        until: str | None = None,
        timeout_ms: int | None = None,
    ) -> Receipt:
        """Navigate and capture DOM + screenshot, gating the capture on a
        readiness state (settle-capture). See :meth:`Session.navigate` for the
        ``until`` / ``timeout_ms`` semantics."""
        r = await self._transport.call(
            "action.web.navigate",
            _build_action_params(
                self.session_id, "navigate", _navigate_payload(url, until, timeout_ms), deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    async def wait_for(
        self,
        *,
        deadline_ms: int = 30000,
        until: str | None = None,
        timeout_ms: int | None = None,
    ) -> Receipt:
        """Wait for the CURRENT page to reach a readiness state (settle-capture),
        without navigating. See :meth:`Session.wait_for` for the ``until`` /
        ``timeout_ms`` semantics."""
        r = await self._transport.call(
            "action.web.wait_for",
            _build_action_params(
                self.session_id, "wait_for", _wait_for_payload(until, timeout_ms), deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    async def close(self) -> SessionInfo:
        try:
            result = await self._transport.call("session.close", {"session_id": self.session_id})
        finally:
            # Always release the socket, even when the RPC fails.
            await self._transport.close()
        if result:
            return SessionInfo._from_dict(result)
        return SessionInfo(self.session_id, "closed", 0)

    async def kill(self) -> None:
        """Force-terminate this session (async).

        Same semantics as ``Session.kill()``: ADMIN ESCAPE HATCH; daemon
        tears down the shim with a 5 s ceiling then SIGKILL. Supports
        :class:`asyncio.CancelledError` integration via the underlying
        transport.
        """
        await self._transport.call("session.kill", {"session_id": self.session_id})

    async def __aenter__(self) -> AsyncSession:
        return self

    async def __aexit__(self, *_: Any) -> None:
        await self.close()
