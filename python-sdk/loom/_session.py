"""The synchronous :class:`Session`."""

from __future__ import annotations

import logging
from typing import Any

from loom._admin import _do_kill_session_sync
from loom._payloads import _build_action_params, _navigate_payload, _wait_for_payload
from loom._transport import LoomTransport
from loom.types import (
    DiffReport,
    ExportInfo,
    Receipt,
    SessionInfo,
    SessionInspection,
    ValidationResult,
)


class Session:
    """
    Synchronous loom session handle.

    Create via ``Session.create()``. Use as a context manager or call
    ``close()`` explicitly when done.
    """

    def __init__(self, session_id: str, status: str, transport: LoomTransport) -> None:
        self.session_id = session_id
        self.status = status
        self._transport = transport

    @classmethod
    def create(
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
    ) -> Session:
        """Create a new session on the daemon and return a Session handle.

        Determinism is ON by default (frozen clock/animations + seeded RNG →
        byte-reproducible captures). Pass ``no_determinism=True`` for
        live/non-reproducible capture; such a session is recorded as
        NON-REPLAYABLE (``replay`` refuses it).

        ``clock_anchor`` is a fixed Unix epoch in milliseconds that pins the
        injected browser clock for cross-run determinism: two recordings with
        the same ``seed`` + ``clock_anchor`` capture identical
        dom/screenshot/outcome hashes, so ``diff`` between them reports zero
        field diffs. Default ``None`` → wall-clock epoch (unchanged
        behavior). No effect under ``no_determinism``.

        ``network_mode`` must be ``"live"`` (the default and only value):
        page traffic is always fetched live from the network — loom does not
        record or replay page-network responses, and response bodies are
        never captured (HAR exports carry no bodies). Anything else
        (including the formerly-accepted-but-inert ``"recorded"`` /
        ``"mixed"``) is rejected by the daemon with ``invalid_network_mode``.
        """
        transport = LoomTransport(socket_path, token)
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
            result = transport.call("session.create", params)
            return cls(
                session_id=result["session_id"],
                status=result.get("status", "active"),
                transport=transport,
            )
        except Exception:
            # Don't leak the connected socket when the RPC fails (schema
            # violation, unknown profile, auth failure, …).
            transport.close()
            raise

    def navigate(
        self,
        url: str,
        *,
        deadline_ms: int = 5000,
        until: str | None = None,
        timeout_ms: int | None = None,
    ) -> Receipt:
        """Navigate and capture DOM + screenshot, gating the capture on a
        readiness state (settle-capture).

        By default loom waits until the page is ``"settled"`` (network quiet +
        ``readyState`` complete + the final URL stable after client-side
        redirects + the DOM quiescent), so the capture is a real rendered page
        rather than a blank SPA shell or an arbitrary animation frame.

        Args:
            until: ``"load" | "networkidle" | "settled"`` (default ``"settled"``).
            timeout_ms: bound on the readiness wait. If readiness is never
                reached (persistent connection, perpetual animation) the call
                still returns — the receipt's ``settle_outcome`` is ``"timeout"``
                / ``"dom_unstable"`` instead of ``"reached"``. It never hangs.
        """
        r = self._transport.call(
            "action.web.navigate",
            _build_action_params(
                self.session_id, "navigate", _navigate_payload(url, until, timeout_ms), deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    def wait_for(
        self,
        *,
        deadline_ms: int = 30000,
        until: str | None = None,
        timeout_ms: int | None = None,
    ) -> Receipt:
        """Wait for the CURRENT page to reach a readiness state (settle-capture),
        without navigating.

        Use after a navigate or an interaction that triggers async re-render to
        gate a subsequent screenshot/snapshot on real readiness instead of a
        magic sleep.

        Args:
            until: ``"load" | "networkidle" | "settled"`` (default ``"settled"``).
            timeout_ms: bound on the wait. If readiness is never reached the call
                still returns — the receipt's ``settle_outcome`` is ``"timeout"``
                / ``"dom_unstable"`` instead of ``"reached"``. It never hangs.
        """
        r = self._transport.call(
            "action.web.wait_for",
            _build_action_params(
                self.session_id, "wait_for", _wait_for_payload(until, timeout_ms), deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    def click(self, selector: str, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.click",
            _build_action_params(self.session_id, "click", {"selector": selector}, deadline_ms),
        )
        return Receipt._from_dict(r)

    def type_text(
        self,
        selector: str,
        text: str,
        *,
        mode: str | None = None,
        deadline_ms: int = 5000,
    ) -> Receipt:
        """Type ``text`` into ``selector``.

        ``mode="fill"`` (default) focuses the element, selects its existing
        content, and commits ``text`` via a single CDP ``Input.insertText`` — a
        genuine (``isTrusted:true``) edit, the same mechanism as Playwright
        ``fill()``. It drives React/react-hook-form ``onChange`` AND is treated
        as user-entered, so trust-gating flows (e.g. Auth0 New Universal Login)
        advance; ``text=""`` clears the field. Date/time-family inputs
        (``date``, ``time``, ``datetime-local``, ``month``, ``week``, ``color``,
        ``range``) are set by value in their own format (``2036-12-31`` for a
        date); a value the input rejects fails with ``malformed_value``, and a
        disabled/readonly target with ``not_editable``. ``mode="value"`` is the legacy
        path: ``.value`` via ``Runtime.evaluate`` + synthetic ``input``/``change``
        events (``isTrusted:false``) — a back-compat escape hatch.
        ``mode="keystrokes"`` dispatches a real per-character CDP
        ``Input.dispatchKeyEvent`` sequence (``isTrusted:true``).
        """
        payload: dict = {"selector": selector, "text": text}
        if mode is not None:
            payload["mode"] = mode
        r = self._transport.call(
            "action.web.type_text",
            _build_action_params(self.session_id, "type_text", payload, deadline_ms),
        )
        return Receipt._from_dict(r)

    def press_key(
        self,
        key: str,
        *,
        selector: str | None = None,
        modifiers: list[str] | None = None,
        deadline_ms: int = 5000,
    ) -> Receipt:
        """Dispatch a real key press (``isTrusted:true``) via CDP
        ``Input.dispatchKeyEvent``.

        ``key`` is a named key (``Enter``, ``Tab``, ``Escape``, arrows, …) or a
        single printable character. ``modifiers`` may include ``Control``,
        ``Alt``, ``Shift``, ``Meta``. With ``selector`` the element is focused
        first; otherwise the event goes to whatever currently has focus.
        """
        payload: dict = {"key": key}
        if selector is not None:
            payload["selector"] = selector
        if modifiers is not None:
            payload["modifiers"] = modifiers
        r = self._transport.call(
            "action.web.press_key",
            _build_action_params(self.session_id, "press_key", payload, deadline_ms),
        )
        return Receipt._from_dict(r)

    def select(self, selector: str, value: str, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.select",
            _build_action_params(
                self.session_id, "select", {"selector": selector, "value": value}, deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    def hover(self, selector: str, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.hover",
            _build_action_params(self.session_id, "hover", {"selector": selector}, deadline_ms),
        )
        return Receipt._from_dict(r)

    def scroll(self, selector: str, *, delta_y: int = 300, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.scroll",
            _build_action_params(
                self.session_id, "scroll", {"selector": selector, "delta_y": delta_y}, deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    def wait(self, selector: str, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.wait",
            _build_action_params(self.session_id, "wait", {"selector": selector}, deadline_ms),
        )
        return Receipt._from_dict(r)

    def evaluate(self, expression: str, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.evaluate",
            _build_action_params(
                self.session_id, "evaluate", {"expression": expression}, deadline_ms
            ),
        )
        return Receipt._from_dict(r)

    def screenshot(self, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.screenshot",
            _build_action_params(self.session_id, "screenshot", {}, deadline_ms),
        )
        return Receipt._from_dict(r)

    def start_recording(
        self,
        *,
        max_duration_ms: int | None = None,
        max_bytes: int | None = None,
        frame_rate: int | None = None,
        deadline_ms: int = 5000,
    ) -> Receipt:
        """Start recording a video (screencast) of the page. Bracket a sequence
        of actions between ``start_recording()`` and ``stop_recording()``; the
        latter returns the ``.webm`` content hash. Caps (all optional, safe
        defaults) auto-stop the recording: ``max_duration_ms`` (300000),
        ``max_bytes`` (268435456), ``frame_rate`` (10).

        NOTE: a recording captures whatever is on screen — including any
        passwords or PII rendered during the window (same posture as
        ``screenshot()``)."""
        payload: dict = {}
        if max_duration_ms is not None:
            payload["max_duration_ms"] = max_duration_ms
        if max_bytes is not None:
            payload["max_bytes"] = max_bytes
        if frame_rate is not None:
            payload["frame_rate"] = frame_rate
        r = self._transport.call(
            "action.web.start_recording",
            _build_action_params(self.session_id, "start_recording", payload, deadline_ms),
        )
        return Receipt._from_dict(r)

    def stop_recording(self, *, deadline_ms: int = 120000) -> Receipt:
        """Stop the active recording, encode it to ``.webm``, and return a
        Receipt whose ``screencast_after_hash`` points at the video in CAS.
        The default deadline is generous because the encode runs synchronously.
        A best-effort encode failure returns an error receipt (the session is
        unaffected)."""
        r = self._transport.call(
            "action.web.stop_recording",
            _build_action_params(self.session_id, "stop_recording", {}, deadline_ms),
        )
        return Receipt._from_dict(r)

    def save_recording(self, receipt: Receipt, path: str) -> None:
        """Fetch the recorded ``.webm`` referenced by a ``stop_recording()``
        receipt and write it to ``path``. Raises if the receipt carries no
        ``screencast_after_hash`` (e.g. the encode failed)."""
        if not receipt.screencast_after_hash:
            raise ValueError("receipt has no screencast_after_hash (recording failed?)")
        content = self._transport.call(
            "content.get", {"artifact_ref": receipt.screencast_after_hash}
        )
        data_hex = content["data_hex"] if isinstance(content, dict) else content
        with open(path, "wb") as f:
            f.write(bytes.fromhex(data_hex))

    def inject_audio(
        self,
        *,
        blob_ref: str | None = None,
        audio_b64: str | None = None,
        await_playout: bool = False,
        deadline_ms: int = 30000,
    ) -> Receipt:
        """Inject caller-provided audio into the session's synthetic
        microphone (``--audio`` sessions only). Provide the payload as
        ``blob_ref`` (a CAS hash, resolved daemon-side — preferred) or
        ``audio_b64`` (inline base64 for short clips). Returns when the
        buffer is enqueued; pass ``await_playout=True`` to return when
        playout completes."""
        payload: dict = {}
        if blob_ref is not None:
            payload["blob_ref"] = blob_ref
        if audio_b64 is not None:
            payload["audio_b64"] = audio_b64
        if await_playout:
            payload["await_playout"] = await_playout
        r = self._transport.call(
            "action.web.inject_audio",
            _build_action_params(self.session_id, "inject_audio", payload, deadline_ms),
        )
        return Receipt._from_dict(r)

    def start_audio_capture(
        self,
        *,
        max_duration_ms: int | None = None,
        max_bytes: int | None = None,
        deadline_ms: int = 5000,
    ) -> Receipt:
        """Start capturing the session's inbound WebRTC audio. Caps
        (optional, safe defaults) truncate rather than error:
        ``max_duration_ms``, ``max_bytes``."""
        payload: dict = {}
        if max_duration_ms is not None:
            payload["max_duration_ms"] = max_duration_ms
        if max_bytes is not None:
            payload["max_bytes"] = max_bytes
        r = self._transport.call(
            "action.web.start_audio_capture",
            _build_action_params(self.session_id, "start_audio_capture", payload, deadline_ms),
        )
        return Receipt._from_dict(r)

    def stop_audio_capture(self, *, deadline_ms: int = 30000) -> Receipt:
        """Stop the active audio capture and return a Receipt whose
        ``audio_after_hash`` points at the 16 kHz mono WAV in CAS. When
        ``audio_stop_reason`` is ``byte_cap``/``duration_cap`` the capture
        was truncated at a cap and a warning is logged — silent truncation
        is a trust failure."""
        r = self._transport.call(
            "action.web.stop_audio_capture",
            _build_action_params(self.session_id, "stop_audio_capture", {}, deadline_ms),
        )
        receipt = Receipt._from_dict(r)
        if receipt.audio_stop_reason in ("byte_cap", "duration_cap"):
            logging.getLogger("loom").warning(
                "audio capture truncated at %s — raise max_bytes/max_duration_ms "
                "on start_audio_capture to keep more",
                receipt.audio_stop_reason,
            )
        return receipt

    def fetch_audio_capture(self, receipt: Receipt) -> bytes:
        """Fetch the captured WAV referenced by a ``stop_audio_capture()``
        receipt and return its bytes. Raises ``ValueError`` if the receipt
        carries no ``audio_after_hash`` (capture errored or nothing was
        captured)."""
        if not receipt.audio_after_hash:
            raise ValueError("receipt has no audio_after_hash — did stop_audio_capture succeed?")
        content = self._transport.call(
            "content.get", {"artifact_ref": receipt.audio_after_hash}
        )
        data_hex = content.get("data_hex") if isinstance(content, dict) else content
        if not isinstance(data_hex, str):
            raise ValueError("content.get response missing data_hex")
        return bytes.fromhex(data_hex)

    def save_audio_capture(self, receipt: Receipt, path: str) -> None:
        """Fetch the captured WAV referenced by a ``stop_audio_capture()``
        receipt and write it to ``path`` as a playable file (overwrites an
        existing file)."""
        with open(path, "wb") as f:
            f.write(self.fetch_audio_capture(receipt))

    def snapshot(self, *, deadline_ms: int = 5000) -> Receipt:
        r = self._transport.call(
            "action.web.snapshot",
            _build_action_params(self.session_id, "snapshot", {}, deadline_ms),
        )
        return Receipt._from_dict(r)

    def close(self) -> SessionInfo:
        try:
            result = self._transport.call("session.close", {"session_id": self.session_id})
        finally:
            # Always release the socket, even when the RPC fails.
            self._transport.close()
        if result:
            return SessionInfo._from_dict(result)
        return SessionInfo(self.session_id, "closed", 0)

    def abort(self, reason: str) -> SessionInfo:
        result = self._transport.call(
            "session.abort", {"session_id": self.session_id, "reason": reason}
        )
        return SessionInfo._from_dict(result)

    def kill(self) -> None:
        """Force-terminate this session.

        Use ``close()`` for normal shutdown. Use ``abort()`` to cancel
        in-flight actions while keeping the session. ``kill()`` is an
        ADMIN ESCAPE HATCH — the daemon tears down the shim with a 5 s
        ceiling then SIGKILL.
        """
        _do_kill_session_sync(self._transport, self.session_id)

    def inspect(self, *, at_action: int | None = None) -> SessionInspection:
        params: dict[str, Any] = {"session_id": self.session_id}
        if at_action is not None:
            params["at_action"] = at_action
        result = self._transport.call("session.inspect", params)
        return SessionInspection._from_dict(result)

    def export(self, format: str) -> ExportInfo:
        result = self._transport.call(
            "session.export", {"session_id": self.session_id, "format": format}
        )
        return ExportInfo._from_dict(result)

    def validate(self) -> ValidationResult:
        result = self._transport.call("session.validate", {"session_id": self.session_id})
        return ValidationResult._from_dict(result)

    def replay(self, *, speed: float = 1.0) -> SessionInfo:
        # No network_mode kwarg here: earlier SDKs sent an inert
        # ``network_mode: "replay"`` the daemon ignored — replay re-executes
        # from the recorded manifest and has no page-network mode to choose.
        result = self._transport.call(
            "session.replay",
            {"session_id": self.session_id, "speed": speed},
        )
        return SessionInfo._from_dict(result)

    def diff(
        self,
        other_session_id: str,
        *,
        include_screenshots: bool = False,
        show_dom_diffs: bool = False,
    ) -> DiffReport:
        result = self._transport.call(
            "session.diff",
            {
                "a": self.session_id,
                "b": other_session_id,
                "include_screenshots": include_screenshots,
                "show_dom_diffs": show_dom_diffs,
            },
        )
        return DiffReport._from_dict(result)

    def __enter__(self) -> Session:
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()
