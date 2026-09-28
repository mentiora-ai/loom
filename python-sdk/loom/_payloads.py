"""Request payloads shared by the sync and async sessions."""

from __future__ import annotations


def _build_action_params(session_id: str, kind: str, payload: dict, deadline_ms: int) -> dict:
    import json

    return {
        "session_id": session_id,
        "action": {
            "kind": kind,
            "payload": list(json.dumps(payload).encode("utf-8")),
            "deadline_ms": deadline_ms,
        },
    }


def _navigate_payload(url: str, until: str | None, timeout_ms: int | None) -> dict:
    """Build the web.navigate action payload, omitting settle-capture options
    when unset so the daemon applies its defaults (until="settled")."""
    payload: dict = {"url": url}
    if until is not None:
        payload["until"] = until
    if timeout_ms is not None:
        payload["timeout_ms"] = timeout_ms
    return payload


def _wait_for_payload(until: str | None, timeout_ms: int | None) -> dict:
    """Build the web.wait_for action payload, omitting options when unset so the
    daemon applies its defaults (until="settled")."""
    payload: dict = {}
    if until is not None:
        payload["until"] = until
    if timeout_ms is not None:
        payload["timeout_ms"] = timeout_ms
    return payload
