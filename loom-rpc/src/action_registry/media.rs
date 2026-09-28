//! The audio and recording verbs: inject_audio, say, audio capture, screencast recording.

use super::action_registry::{ActionMeta, ParamMeta, ParamType, DEADLINE_MS_PARAM};

pub(super) const WEB_INJECT_AUDIO: ActionMeta = ActionMeta {
        name: "web.inject_audio",
        summary: "Inject caller-provided audio into the page's virtual microphone.",
        description: "\
Feeds caller-provided audio into the page as if it arrived from the \
microphone, so a browser voice agent under test 'hears' a scripted \
utterance. The payload is supplied EITHER inline as base64 (`audio_b64`) \
OR by content-store reference (`blob_ref`) — exactly one — and the daemon \
resolves and size-bounds it before dispatch.\n\n\
The call returns once the audio is ENQUEUED, not when playout completes; \
pass `await_playout: true` (JSON boolean) to wait for the source node to \
end. This registry entry SURFACES the verb (MCP `tools/list`, `action.web.*` \
alias, docs); the injection itself is wired in a later increment and is only \
meaningful on an audio-enabled session.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "blob_ref",
                ty: ParamType::String,
                doc: "Optional content-store hash of the audio payload (mutually exclusive with `audio_b64`).",
                required: false,
            },
            ParamMeta {
                name: "audio_b64",
                ty: ParamType::String,
                doc: "Optional base64-encoded audio payload, inline (mutually exclusive with `blob_ref`); size-bounded before decode.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. `outcome_hash` is a constant enqueue-success marker; playout completion (when `await_playout` is set) is a receipt field and is never hashed.",
        example: &["loom", "action", "web.inject_audio", "--session", "<SESSION>"],
};

pub(super) const WEB_SAY: ActionMeta = ActionMeta {
        name: "web.say",
        summary: "Speak text into the page's microphone via a configured external TTS backend.",
        description: "\
Synthesizes `text` to speech through an operator-configured external TTS \
backend and injects the result into the page's virtual microphone — the \
spoken equivalent of `web.inject_audio`. loom ships NO TTS engine; the \
operator wires one via environment (a command or URL), and the synthesized \
bytes flow through the same injection path and size bounds as any other \
audio payload.\n\n\
The call returns on ENQUEUE; pass `await_playout: true` (JSON boolean) to \
wait for playout to finish. This is a P1, feature-gated verb; this registry \
entry SURFACES it (MCP `tools/list`, `action.web.*` alias, docs) while the \
TTS backend and injection are wired in later increments.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "text",
                ty: ParamType::String,
                doc: "The text to synthesize and speak into the page microphone. Length-bounded by the TTS backend.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"`. `outcome_hash` is a constant enqueue-success marker. When no TTS backend is configured, a typed error names both backend environment variables.",
        example: &["loom", "action", "web.say", "--session", "<SESSION>", "--text", "hello there"],
};

pub(super) const WEB_START_AUDIO_CAPTURE: ActionMeta = ActionMeta {
        name: "web.start_audio_capture",
        summary: "Begin capturing inbound call audio into a bounded in-page buffer.",
        description: "\
Starts recording the INBOUND audio of a WebRTC call on the session's page — \
the remote participant(s), not the injected microphone — into a bounded \
in-page ring buffer. Paired with `web.stop_audio_capture`, which returns the \
buffered audio as a WAV content reference. `max_duration_ms` and `max_bytes` \
cap the capture; on a cap hit the capture truncates (never errors) and the \
stop reports the reason.\n\n\
At most one capture is active per session; the injected microphone track is \
excluded from the capture by provenance so a session never records its own \
injected audio. This registry entry SURFACES the verb; the capture tap is \
wired in a later increment and requires an audio-enabled session.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "max_duration_ms",
                ty: ParamType::U64,
                doc: "Optional capture duration cap in milliseconds; on expiry the capture truncates and stop reports `duration_cap`. Omit or 0 for a safe default.",
                required: false,
            },
            ParamMeta {
                name: "max_bytes",
                ty: ParamType::U64,
                doc: "Optional captured-bytes cap; on hit the capture truncates and stop reports `byte_cap`. Omit or 0 for a safe default.",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `status: \"ok\"` acknowledging capture start. The captured audio is returned by `web.stop_audio_capture`, not here.",
        example: &["loom", "action", "web.start_audio_capture", "--session", "<SESSION>"],
};

pub(super) const WEB_START_RECORDING: ActionMeta = ActionMeta {
        name: "web.start_recording",
        summary: "Start recording a video (screencast) of the page.",
        description: "\
Starts a CDP `Page.startScreencast` recording on the session's active page \
target. Frames are collected and acked until `web.stop_recording` is called \
(or a cap is hit), then encoded to a `.webm` (VP8/VP9) via a bundled ffmpeg. \
At most one recording is active per session; calling this while a recording \
is already running fails with `kind: \"js_throw\"`.\n\n\
Resource caps (all optional, safe defaults): `max_duration_ms` (default \
300000), `max_bytes` (default 268435456), `frame_rate` (default 10). The \
recording auto-stops when any cap is reached. Frames spill to a temp file so \
a long recording does not hold the whole video in memory.\n\n\
PRIVACY: a recording captures whatever is on screen, INCLUDING any passwords, \
PII, or third-party content rendered during the window — same posture as \
`web.screenshot`. The `.webm` is stored in the local content-addressed store; \
its bytes are excluded from the replay hash chain (only the content hash is \
recorded), so recording never affects replay-equality.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            ParamMeta {
                name: "max_duration_ms",
                ty: ParamType::U64,
                doc: "Optional auto-stop after this many ms (default 300000 = 5 min).",
                required: false,
            },
            ParamMeta {
                name: "max_bytes",
                ty: ParamType::U64,
                doc: "Optional auto-stop once the buffered frames exceed this many bytes (default 268435456 = 256 MiB).",
                required: false,
            },
            ParamMeta {
                name: "frame_rate",
                ty: ParamType::U64,
                doc: "Optional ENCODE/playback frames-per-second for the output .webm (default 10, clamped 1..=60). This is the muxed output rate; the browser still emits a frame per visual change (it does not decimate capture).",
                required: false,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt confirming the recording started (`code: web_action_completed`). The video hash is returned by `web.stop_recording`.",
        example: &["loom", "action", "web.start_recording", "--session", "<SESSION>"],
};

pub(super) const WEB_STOP_AUDIO_CAPTURE: ActionMeta = ActionMeta {
        name: "web.stop_audio_capture",
        summary: "Stop inbound audio capture; return the buffered audio as a WAV reference.",
        description: "\
Stops the active inbound-audio capture started by `web.start_audio_capture`, \
drains the in-page buffer, resamples to 16 kHz mono, and returns the result \
as a WAV content reference plus a `stop_reason` (explicit, a cap hit, no \
inbound track, or session close). The returned reference is fetchable to a \
playable `.wav` via `loom blob get`.\n\n\
The captured audio is OBSERVATIONAL — it is excluded from the replay hash \
chain, so a voice session must run with determinism disabled. This registry \
entry SURFACES the verb; the drain, resample, and WAV mux are wired in a \
later increment. Calling it without an active capture is a typed error.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `audio_after_hash` (or an audio blob reference; fetch via `loom blob get <hash>`) and `stop_reason` (one of explicit, byte_cap, duration_cap, no_samples, no_inbound_track, session_closed, error).",
        example: &["loom", "action", "web.stop_audio_capture", "--session", "<SESSION>"],
};

pub(super) const WEB_STOP_RECORDING: ActionMeta = ActionMeta {
        name: "web.stop_recording",
        summary: "Stop the active video recording and return its content hash.",
        description: "\
Stops the recording started by `web.start_recording`, encodes the collected \
frames to a `.webm`, stores it in the content-addressed store, and returns the \
content hash. Fails with `kind: \"js_throw\"` if no recording is active, or if \
the recording captured zero frames.\n\n\
If the bundled ffmpeg encoder is unavailable (download failed / offline / the \
`video` build feature is off) the action returns an error receipt with an \
actionable message, but the session itself is unaffected — recording is \
best-effort and never aborts the session.",
        params: &[
            ParamMeta {
                name: "session_id",
                ty: ParamType::String,
                doc: "Session created via `loom session create`. 26-char ULID format.",
                required: true,
            },
            DEADLINE_MS_PARAM,
        ],
        returns: "Receipt with `screencast_after_hash: <sha256>` pointing to the `.webm` in CAS — fetch via `loom blob get <hash>`. A best-effort encode failure (ffmpeg unavailable / zero frames) returns an error receipt (`kind: \"recording_failed\"`) without aborting the session.",
        example: &["loom", "action", "web.stop_recording", "--session", "<SESSION>"],
};
