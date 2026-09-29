"""
loom — Python client library for the loom browser-automation daemon.

Quick start::

    import loom

    # Synchronous
    with loom.Session.create() as session:
        receipt = session.navigate("https://example.com")

    # Asynchronous
    import asyncio

    async def main():
        async with await loom.AsyncSession.create() as session:
            receipt = await session.navigate("https://example.com")

    asyncio.run(main())
"""

from __future__ import annotations

# ``name as name`` marks a re-export that ``__all__`` deliberately leaves out.
from loom._admin import (
    daemon_health,
    kill_session,
)
from loom._admin import (
    session_list as session_list,
)
from loom._admin import (
    vault_grant as vault_grant,
)
from loom._admin import (
    vault_list_grants as vault_list_grants,
)
from loom._admin import (
    vault_revoke as vault_revoke,
)
from loom._async_session import AsyncSession
from loom._async_transport import AsyncLoomTransport as AsyncLoomTransport
from loom._errors import LoomConnectionError, LoomError, LoomRPCError, LoomTokenError
from loom._session import Session
from loom._transport import LoomTransport as LoomTransport
from loom.types import (
    DiffReport,
    ExportInfo,
    GrantInfo,
    LoomErrorCode,
    Receipt,
    ReceiptError,
    SchemaRegistry,
    SessionInfo,
    SessionInspection,
    ValidationResult,
)

# Single source of truth for the package version: pyproject.toml reads this via
# [tool.hatch.version], and the publish workflow asserts it matches the release tag.
__version__ = "0.15.7"
__all__ = [
    "Session",
    "AsyncSession",
    "LoomError",
    "LoomRPCError",
    "LoomConnectionError",
    "LoomTokenError",
    "SessionInfo",
    "SessionInspection",
    "Receipt",
    "ReceiptError",
    "DiffReport",
    "ExportInfo",
    "ValidationResult",
    "GrantInfo",
    "SchemaRegistry",
    "LoomErrorCode",
    "kill_session",
    "daemon_health",
]
