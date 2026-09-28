"""
Connected-socket cleanup: Session.create / Session.close (sync and async)
must always release their transport when the wrapped RPC fails, instead of
leaking the connected Unix socket.
"""

from __future__ import annotations

import pytest

import loom
import loom._async_session
import loom._session
from loom._async_transport import AsyncLoomTransport
from loom._errors import LoomRPCError
from loom._transport import LoomTransport


def _fail(_params: dict) -> dict:
    raise ValueError("boom")  # MockDaemon converts this to an internal_error envelope


def test_session_create_failure_closes_transport(daemon, monkeypatch):
    created: list[LoomTransport] = []

    class RecordingTransport(LoomTransport):
        def __init__(self, *args, **kwargs) -> None:
            super().__init__(*args, **kwargs)
            created.append(self)

    # Patch where Session.create looks the transport up.
    monkeypatch.setattr(loom._session, "LoomTransport", RecordingTransport)
    daemon.register_handler("session.create", _fail)
    with pytest.raises(LoomRPCError):
        loom.Session.create(socket_path=str(daemon.socket_path), token=daemon.token)
    assert len(created) == 1
    # socket.close() detaches the fd (fileno() == -1) — the socket was released.
    assert created[0]._sock.fileno() == -1


def test_session_close_failure_still_closes_transport(daemon):
    daemon.register_handler("session.close", _fail)
    s = loom.Session.create(socket_path=str(daemon.socket_path), token=daemon.token)
    with pytest.raises(LoomRPCError):
        s.close()
    assert s._transport._sock.fileno() == -1


async def test_async_session_create_failure_closes_transport(daemon, monkeypatch):
    created: list[AsyncLoomTransport] = []

    class RecordingTransport(AsyncLoomTransport):
        def __init__(self, *args, **kwargs) -> None:
            super().__init__(*args, **kwargs)
            created.append(self)

    monkeypatch.setattr(loom._async_session, "AsyncLoomTransport", RecordingTransport)
    daemon.register_handler("session.create", _fail)
    with pytest.raises(LoomRPCError):
        await loom.AsyncSession.create(socket_path=str(daemon.socket_path), token=daemon.token)
    assert len(created) == 1
    assert created[0]._closed is True


async def test_async_session_close_failure_still_closes_transport(daemon):
    daemon.register_handler("session.close", _fail)
    s = await loom.AsyncSession.create(socket_path=str(daemon.socket_path), token=daemon.token)
    with pytest.raises(LoomRPCError):
        await s.close()
    assert s._transport._closed is True
