//! voice-call-io (task 09): `web.say` TTS backend (P1, feature-gated).
//!
//! Synthesizes `text` → audio bytes via an operator-configured external backend,
//! then the caller (`wasm_bridge`'s `WebSay` arm) feeds the bytes through the
//! ordinary `web.inject_audio` path — so `say` inherits every inject bound (the
//! 8 MiB size cap, the determinism gate, replay exclusion). loom ships NO TTS
//! engine; the operator wires one via environment.
//!
//! Two backends, both hardened per PRD D7 / Architecture §6 / decisions-arch
//! A9/A10/A14:
//! - `LOOM_TTS_CMD` — a JSON argv array (`["espeak-ng","--stdout"]`). Exec'd as
//!   an argv vector via `Command::new(argv[0]).args(&argv[1..])`, **never `sh -c`**;
//!   `text` is written **only to stdin**, never interpolated into the command.
//! - `LOOM_TTS_URL` — an **https** endpoint. `POST text/plain`; SSRF-guarded
//!   (scheme allowlist, single DNS resolve + pinned connection, **redirects
//!   refused**), copied-and-tightened from `loom-host`'s `net_request` guard.
//!
//! Both: 5 s timeout, bounded stdout/stderr / response body, SIGKILL / drop on
//! expiry (no zombies, no pipe deadlock). Neither env set → a typed error naming
//! **both** vars. Kill switch `LOOM_DISABLE_TTS=1`.
//!
//! Module scope is deliberately small (intake council FND-0001): `from_env` +
//! `synthesize` + the pure SSRF predicates. Not a plugin framework.

// NOTE (decisions.md D2 / intake FND-0005): the SSRF predicates below are COPIED
// from `loom-host/src/host_function_table/host_impl.rs` (the `net_request` guard)
// and TIGHTENED to https-only + reject-all-redirects. They are duplicated (not
// imported) because those helpers are module-private to loom-host and this task's
// boundary is loom-daemon only; extraction to a shared crate is a filed cleanup.
// Keep the two in sync — the pure predicates are unit-tested here independently.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// The synthesized-output cap = the inject cap (A14): TTS output flows through the
// same `inject_audio` bound as any payload. Single source of truth in `wasm_bridge`.
use crate::inject_payload::MAX_INJECT_BYTES;

/// Max `text` length accepted (UTF-8 bytes), checked before spawn/POST (PRD D7
/// "text length is capped").
pub const MAX_TTS_TEXT_BYTES: usize = 4096;

/// Default per-backend wall-clock budget in ms (PRD D13: "TTS subprocess / HTTP —
/// 5 s → typed `tts_failed` (SIGKILL)"). PRD D7 says "5 s **default**"; overridable
/// via `LOOM_TTS_TIMEOUT_MS` (plan-council P-A2 — heavy local models exceed 5 s),
/// clamped to [`TTS_TIMEOUT_MS_MIN`, `TTS_TIMEOUT_MS_MAX`].
pub const TTS_TIMEOUT_MS_DEFAULT: u64 = 5_000;
pub const TTS_TIMEOUT_MS_MIN: u64 = 100;
pub const TTS_TIMEOUT_MS_MAX: u64 = 120_000;

/// Resolve the per-backend timeout from `LOOM_TTS_TIMEOUT_MS` (clamped), default 5 s.
fn tts_timeout() -> std::time::Duration {
    let ms = std::env::var("LOOM_TTS_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.clamp(TTS_TIMEOUT_MS_MIN, TTS_TIMEOUT_MS_MAX))
        .unwrap_or(TTS_TIMEOUT_MS_DEFAULT);
    std::time::Duration::from_millis(ms)
}

/// Resolved backend selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsBackend {
    /// `LOOM_TTS_CMD` — a non-empty argv vector.
    Cmd(Vec<String>),
    /// `LOOM_TTS_URL` — an https endpoint string.
    Url(String),
    /// Neither configured.
    Unconfigured,
}

/// Typed TTS failure. Each maps to a receipt `error.kind` via [`TtsError::kind`].
#[derive(Debug)]
pub enum TtsError {
    /// Neither `LOOM_TTS_CMD` nor `LOOM_TTS_URL` set. Message names BOTH (AC6).
    NotConfigured,
    /// `LOOM_DISABLE_TTS=1`.
    Disabled,
    /// Malformed `LOOM_TTS_CMD` (not a JSON string array / empty) or unparseable URL.
    ConfigInvalid(String),
    /// SSRF policy rejected the URL (scheme / private-IP / DNS). Carries the JSON reason.
    UrlBlocked(String),
    /// `text` exceeds [`MAX_TTS_TEXT_BYTES`].
    TextTooLong { len: usize, cap: usize },
    /// D7/D13 umbrella: non-zero exit, 5 s timeout (SIGKILL), non-2xx after retry,
    /// wrong content-type, over-cap, or empty output. Message = the specific cause.
    Failed(String),
}

impl TtsError {
    /// The receipt `error.kind` string.
    pub fn kind(&self) -> &'static str {
        match self {
            TtsError::NotConfigured => "tts_not_configured",
            TtsError::Disabled => "tts_disabled",
            TtsError::ConfigInvalid(_) => "tts_config_invalid",
            TtsError::UrlBlocked(_) => "url_blocked",
            TtsError::TextTooLong { .. } => "invalid_argument",
            TtsError::Failed(_) => "tts_failed",
        }
    }
}

impl std::fmt::Display for TtsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TtsError::NotConfigured => write!(
                f,
                "no TTS backend configured: set LOOM_TTS_CMD (a JSON argv array, \
                 e.g. [\"espeak-ng\",\"--stdout\"]) or LOOM_TTS_URL (an https endpoint)"
            ),
            TtsError::Disabled => write!(f, "TTS is disabled (LOOM_DISABLE_TTS=1)"),
            TtsError::ConfigInvalid(m) => write!(f, "invalid TTS configuration: {m}"),
            TtsError::UrlBlocked(m) => write!(f, "TTS URL blocked by SSRF policy: {m}"),
            TtsError::TextTooLong { len, cap } => {
                write!(f, "say text is {len} bytes, over the {cap}-byte cap")
            }
            TtsError::Failed(m) => write!(f, "TTS backend failed: {m}"),
        }
    }
}

/// `true` when `LOOM_DISABLE_TTS` is set to `1`/`true` (kill switch, PRD D7),
/// mirroring the `LOOM_DISABLE_AUDIO` idiom (`audio_bridge.rs`).
fn tts_disabled() -> bool {
    matches!(std::env::var("LOOM_DISABLE_TTS"), Ok(v) if v == "1" || v.eq_ignore_ascii_case("true"))
}

impl TtsBackend {
    /// Resolve the backend from the environment. Setting BOTH `LOOM_TTS_CMD` and
    /// `LOOM_TTS_URL` is a hard error (`ConfigInvalid`, "set exactly one") — fail
    /// loud on ambiguous config rather than silently pick a winner (plan-council
    /// P-A1). `LOOM_TTS_CMD` must be a non-empty JSON array of strings; a malformed
    /// value → `ConfigInvalid`. Neither set → `Ok(Unconfigured)`.
    pub fn from_env() -> Result<Self, TtsError> {
        let cmd = std::env::var("LOOM_TTS_CMD").ok().filter(|s| !s.is_empty());
        let url = std::env::var("LOOM_TTS_URL").ok().filter(|s| !s.is_empty());
        match (cmd, url) {
            (Some(_), Some(_)) => Err(TtsError::ConfigInvalid(
                "set exactly one of LOOM_TTS_CMD or LOOM_TTS_URL, not both".to_string(),
            )),
            (Some(cmd), None) => {
                // Argv is a JSON string array (decisions D1) — unambiguous, no shell,
                // no whitespace-split foot-gun (intake FND-0002/0026).
                let argv: Vec<String> = serde_json::from_str(&cmd).map_err(|e| {
                    TtsError::ConfigInvalid(format!(
                        "LOOM_TTS_CMD must be a JSON array of strings (e.g. \
                         [\"espeak-ng\",\"--stdout\"]): {e}"
                    ))
                })?;
                if argv.is_empty() || argv[0].is_empty() {
                    return Err(TtsError::ConfigInvalid(
                        "LOOM_TTS_CMD must be a non-empty JSON argv array".to_string(),
                    ));
                }
                Ok(TtsBackend::Cmd(argv))
            }
            (None, Some(url)) => Ok(TtsBackend::Url(url)),
            (None, None) => Ok(TtsBackend::Unconfigured),
        }
    }
}

/// Synthesize `text` to audio bytes via the operator-configured backend.
///
/// Order: `LOOM_DISABLE_TTS` gate → text-length gate → backend dispatch. The
/// returned bytes are the raw synthesized audio (WAV/mp3/opus — whatever the
/// backend emits); the caller size-checks them against the inject cap and hands
/// them to `inject_audio`. `text` is NEVER logged (plan-council P-A3 — it can be
/// sensitive call audio); only its length and the failure class are observable.
pub async fn synthesize(text: &str) -> Result<Vec<u8>, TtsError> {
    if tts_disabled() {
        return Err(TtsError::Disabled);
    }
    if text.len() > MAX_TTS_TEXT_BYTES {
        return Err(TtsError::TextTooLong {
            len: text.len(),
            cap: MAX_TTS_TEXT_BYTES,
        });
    }
    match TtsBackend::from_env()? {
        TtsBackend::Unconfigured => Err(TtsError::NotConfigured),
        TtsBackend::Cmd(argv) => run_cmd(&argv, text).await,
        TtsBackend::Url(url) => post_url(&url, text).await,
    }
}

// ----- Cmd backend -----

/// Exec an argv vector, write `text` to stdin, return bounded stdout. `text` is
/// never placed in argv (D7). The stdin writer runs on its own task so a child
/// that fills its stdout pipe while we are still writing stdin cannot deadlock;
/// stdout/stderr are bounded reads. `kill_on_drop(true)` + `timeout` means a
/// child that overruns the (configurable) budget is SIGKILLed on drop and reaped
/// by tokio (no zombie). Known limitation (FND-0013): SIGKILL hits the DIRECT
/// child; a shell wrapper that forks background grandchildren could leak them —
/// full process-group kill needs `libc::kill(-pgid)` (not a daemon dep), deferred.
async fn run_cmd(argv: &[String], text: &str) -> Result<Vec<u8>, TtsError> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| TtsError::Failed(format!("could not exec {}: {e}", argv[0])))?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let text_bytes = text.as_bytes().to_vec();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&text_bytes).await;
        let _ = stdin.shutdown().await; // EOF so the child stops reading
    });

    // Drain stderr in ITS OWN task so it never gates the stdout read. Content is
    // intentionally discarded — it can echo the sensitive spoken text, so it is never
    // logged or surfaced (P-A3 / FND-0012). Draining still matters: without it a child
    // that writes a lot of stderr would block on a full stderr pipe. (An earlier
    // version `join!`ed the two reads, which deadlocked the over-cap case — the stdout
    // reader hit its cap and stopped, the child blocked writing the rest, and the
    // stderr read then waited for an EOF that never came, hanging until the timeout;
    // FND-0003.)
    let stderr_drainer = tokio::spawn(async move {
        let mut sink = Vec::new();
        let _ = (&mut stderr).take(8 * 1024).read_to_end(&mut sink).await;
    });

    let run = async move {
        let mut out = Vec::new();
        // Cap stdout at the inject bound + 1 so we can detect an over-cap producer.
        let mut out_reader = (&mut stdout).take(MAX_INJECT_BYTES as u64 + 1);
        out_reader
            .read_to_end(&mut out)
            .await
            .map_err(|e| TtsError::Failed(format!("reading TTS stdout: {e}")))?;
        // Fail fast on an over-cap producer: bail as soon as the cap is hit, do NOT
        // `wait()` — once we stop reading a full stdout pipe the child blocks on write,
        // so `wait()` would hang until the timeout (FND-0003). Returning drops `child`
        // → kill_on_drop SIGKILLs it.
        if out.len() > MAX_INJECT_BYTES {
            return Err(TtsError::Failed(format!(
                "TTS output exceeds the {MAX_INJECT_BYTES}-byte cap"
            )));
        }
        let status = child
            .wait()
            .await
            .map_err(|e| TtsError::Failed(format!("awaiting TTS command: {e}")))?;
        Ok::<(std::process::ExitStatus, Vec<u8>), TtsError>((status, out))
    };

    let outcome = tokio::time::timeout(tts_timeout(), run).await;
    let _ = writer.await; // best-effort join; on timeout the child is already dead
    stderr_drainer.abort(); // content discarded; stop draining regardless of outcome

    let (status, out) = match outcome {
        Err(_elapsed) => {
            // `run` (owning `child`) was dropped → kill_on_drop SIGKILLed it.
            tracing::warn!(argv0 = %argv[0], "tts.cmd_timeout");
            return Err(TtsError::Failed(
                "TTS command timed out (SIGKILL)".to_string(),
            ));
        }
        Ok(res) => res?,
    };

    if !status.success() {
        // Log ONLY the exit code + stderr LENGTH — never stderr CONTENT, which can
        // echo the (sensitive) spoken text (P-A3 / ship-council FND-0012). The receipt
        // likewise carries only the exit class.
        tracing::warn!(code = status.code(), "tts.cmd_nonzero_exit");
        let code = status
            .code()
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "terminated by signal".to_string());
        return Err(TtsError::Failed(format!("TTS command failed ({code})")));
    }
    if out.is_empty() {
        return Err(TtsError::Failed(
            "TTS command produced no output".to_string(),
        ));
    }
    Ok(out)
}

// ----- Url backend (SSRF-guarded, https-only, no redirects) -----

/// Max retry attempts total (1 initial + 2 retries) on 5xx/429 (A10).
const TTS_HTTP_ATTEMPTS: usize = 3;

/// `POST text/plain` to the operator URL: SSRF-validate, then transport.
///
/// The WHOLE path — DNS resolve + every retry attempt + body read — is bounded by a
/// SINGLE `tts_timeout()` budget (ship-council FND-0001/0010/0015/0020/0024). The
/// per-request reqwest timeout caps one hung attempt, but only this outer bound
/// guarantees the action honors `LOOM_TTS_TIMEOUT_MS` regardless of retry count, and
/// it is the ONLY timeout covering DNS resolution (`lookup_host` has none of its own).
async fn post_url(url: &str, text: &str) -> Result<Vec<u8>, TtsError> {
    let budget = tts_timeout();
    match tokio::time::timeout(budget, async {
        let resolved = resolve_and_validate(url).await?;
        do_post(url, &resolved, text).await
    })
    .await
    {
        Ok(res) => res,
        Err(_elapsed) => Err(TtsError::Failed(format!(
            "TTS URL backend timed out after {} ms",
            budget.as_millis()
        ))),
    }
}

/// Transport: POST `text` to `url`, pinned to the pre-validated `resolved` addrs,
/// with A10 retry (retry on 5xx/429 only; never other 4xx; never for Cmd). One
/// `reqwest::Client` is built and reused across attempts (P-A4). Redirects are
/// refused (any 3xx → `Failed`). The response body is read with a running cap.
///
/// INVARIANT (plan-council P-A7): production callers MUST call
/// [`resolve_and_validate`] first — this function does NOT re-check the SSRF
/// policy (it trusts `resolved`). Tests call it directly against a loopback stub
/// to exercise the transport ONLY; that is not a policy bypass because the sole
/// production caller ([`post_url`]) always validates first.
async fn do_post(url: &str, resolved: &[SocketAddr], text: &str) -> Result<Vec<u8>, TtsError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| TtsError::ConfigInvalid(format!("invalid LOOM_TTS_URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| TtsError::ConfigInvalid("LOOM_TTS_URL has no host".to_string()))?
        .to_owned();

    // Pin the connection to EVERY validated addr so reqwest cannot re-resolve DNS to a
    // different (internal) IP between our check and the connect (TOCTOU, P-A8).
    // `resolve_to_addrs` pins the FULL validated set in one call — a `resolve()` loop
    // would replace the prior override each iteration and pin only the last addr
    // (ship-council FND-0002).
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(tts_timeout())
        .resolve_to_addrs(&host, resolved)
        .build()
        .map_err(|e| TtsError::Failed(format!("building TTS client: {e}")))?;

    let mut last = String::new();
    for attempt in 1..=TTS_HTTP_ATTEMPTS {
        let resp = client
            .post(parsed.clone())
            .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(text.to_string())
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            // Network/timeout errors are not 5xx/429 → not retried (A10). Never log `text`.
            Err(e) => {
                tracing::warn!(attempt, "tts.http_send_error");
                return Err(TtsError::Failed(format!("TTS request failed: {e}")));
            }
        };

        let status = resp.status();
        if status.is_redirection() {
            return Err(TtsError::Failed(format!(
                "TTS backend returned a redirect ({status}); redirects are refused"
            )));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            last = format!("HTTP {status}");
            tracing::warn!(attempt, %status, "tts.http_retryable");
            if attempt < TTS_HTTP_ATTEMPTS {
                // Brief backoff so a flapping backend isn't hammered (FND-0011). The
                // outer `post_url` budget still bounds the total wall-clock.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            return Err(TtsError::Failed(format!(
                "TTS backend failed after {TTS_HTTP_ATTEMPTS} attempts ({last})"
            )));
        }
        if !status.is_success() {
            // Other 4xx → no retry (A10).
            return Err(TtsError::Failed(format!("TTS backend returned {status}")));
        }

        // 2xx: reject an obviously-wrong content-type (an HTML/JSON error page is
        // not audio) — absent / audio/* / application/octet-stream are accepted.
        if let Some(ct) = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        {
            let ct_l = ct.to_ascii_lowercase();
            if ct_l.starts_with("text/") || ct_l.starts_with("application/json") {
                return Err(TtsError::Failed(format!(
                    "TTS backend returned unexpected content-type: {ct}"
                )));
            }
        }
        // Reject a declared over-cap body before reading it.
        if let Some(len) = resp.content_length() {
            if len > MAX_INJECT_BYTES as u64 {
                return Err(TtsError::Failed(format!(
                    "TTS response is {len} bytes, over the {MAX_INJECT_BYTES}-byte cap"
                )));
            }
        }
        let bytes = read_bounded_response(resp, MAX_INJECT_BYTES).await?;
        if bytes.is_empty() {
            return Err(TtsError::Failed(
                "TTS backend returned an empty body".to_string(),
            ));
        }
        return Ok(bytes);
    }
    Err(TtsError::Failed(format!("TTS backend failed ({last})")))
}

/// Stream a response body with a running byte cap so a chunked (Content-Length
/// absent) response cannot OOM the daemon (intake FND-0028).
async fn read_bounded_response(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, TtsError> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| TtsError::Failed(format!("reading TTS response: {e}")))?
    {
        if buf.len() + chunk.len() > cap {
            return Err(TtsError::Failed(format!(
                "TTS response exceeds the {cap}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Resolve `url`'s host to socket addrs (single lookup) and run the SSRF policy.
/// https-only. Returns the validated addrs (to be pinned) on success. A literal-IP
/// host skips DNS. Mirrors `host_impl.rs::resolve_and_validate`, tightened to https.
async fn resolve_and_validate(url: &str) -> Result<Vec<SocketAddr>, TtsError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| TtsError::ConfigInvalid(format!("invalid LOOM_TTS_URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| TtsError::UrlBlocked("{\"reason\":\"missing_host\"}".to_string()))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| TtsError::UrlBlocked("{\"reason\":\"unknown_port\"}".to_string()))?;

    let resolved: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| {
                TtsError::UrlBlocked(format!("{{\"reason\":\"dns_failed\",\"detail\":\"{e}\"}}"))
            })?
            .collect()
    };
    validate_outbound_url(url, &resolved)?;
    Ok(resolved)
}

/// Pure SSRF policy: **https** scheme (tightened vs loom-host's http|https) + every
/// resolved IP outside the blocked set. Side-effect-free so it is unit-tested directly.
fn validate_outbound_url(url: &str, resolved: &[SocketAddr]) -> Result<(), TtsError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| TtsError::ConfigInvalid(format!("invalid LOOM_TTS_URL: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(TtsError::UrlBlocked(format!(
            "{{\"reason\":\"scheme_not_https\",\"scheme\":\"{}\"}}",
            parsed.scheme()
        )));
    }
    if resolved.is_empty() {
        return Err(TtsError::UrlBlocked(
            "{\"reason\":\"dns_no_addresses\"}".to_string(),
        ));
    }
    for addr in resolved {
        if ip_is_blocked(addr.ip()) {
            return Err(TtsError::UrlBlocked(format!(
                "{{\"reason\":\"private_or_loopback_ip\",\"ip\":\"{}\"}}",
                addr.ip()
            )));
        }
    }
    Ok(())
}

/// `true` for IPs a TTS URL must never reach (loopback, RFC1918, link-local incl.
/// 169.254.169.254 metadata, ULA, unspecified, broadcast, doc/benchmark ranges),
/// with IPv4-mapped IPv6 unwrapped. Copied from `host_impl.rs::ip_is_blocked`.
fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_blocked(v4),
        IpAddr::V6(v6) => {
            // Unwrap IPv4-MAPPED (::ffff:a.b.c.d) via `to_ipv4_mapped` (NOT `to_ipv4`,
            // which also maps the deprecated compat form and would turn ::1 into 0.0.0.1).
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ipv4_is_blocked(mapped);
            }
            ipv6_is_blocked(v6)
        }
    }
}

fn ipv4_is_blocked(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0/24 IETF
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18/15 benchmarking
        || o[0] >= 240 // 240/4 reserved
}

fn ipv6_is_blocked(v6: Ipv6Addr) -> bool {
    v6.is_loopback()
        || v6.is_unspecified()
        || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
        || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

    // Serializes env mutation across tests (CLAUDE.md ENV_LOCK mandate). A tokio
    // async Mutex (not std) so the guard is held across `.await` in these
    // #[tokio::test]s without tripping clippy::await_holding_lock, and it never
    // poisons (the repo pattern in screencast_recorder/interface_tests.rs). Each
    // test save/restores the vars it touches so they never leak across tests.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Save/restore guard for the three TTS env vars, so tests are order-independent.
    struct EnvGuard {
        cmd: Option<String>,
        url: Option<String>,
        disable: Option<String>,
    }
    impl EnvGuard {
        fn capture() -> Self {
            Self {
                cmd: std::env::var("LOOM_TTS_CMD").ok(),
                url: std::env::var("LOOM_TTS_URL").ok(),
                disable: std::env::var("LOOM_DISABLE_TTS").ok(),
            }
        }
        fn clear() {
            std::env::remove_var("LOOM_TTS_CMD");
            std::env::remove_var("LOOM_TTS_URL");
            std::env::remove_var("LOOM_DISABLE_TTS");
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            let restore = |k: &str, v: &Option<String>| match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            };
            restore("LOOM_TTS_CMD", &self.cmd);
            restore("LOOM_TTS_URL", &self.url);
            restore("LOOM_DISABLE_TTS", &self.disable);
        }
    }

    /// A one-shot HTTP/1.1 stub: serves scripted `(status, content_type, body)`
    /// responses in order (one per connection), on 127.0.0.1. Returns the bound
    /// addr. Used to exercise `do_post` (the transport), never the SSRF policy.
    fn spawn_http_stub(responses: Vec<(u16, &'static str, Vec<u8>)>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, ctype, body) in responses {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                // Drain the request (best-effort) so the client's write completes.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let reason = if status == 200 { "OK" } else { "ERR" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        addr
    }

    // ---- backend selection / config (AC6) ----

    #[tokio::test]
    async fn unconfigured_names_both_env_vars() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        let err = synthesize("hello").await.unwrap_err();
        assert_eq!(err.kind(), "tts_not_configured");
        let msg = err.to_string();
        assert!(
            msg.contains("LOOM_TTS_CMD"),
            "must name LOOM_TTS_CMD: {msg}"
        );
        assert!(
            msg.contains("LOOM_TTS_URL"),
            "must name LOOM_TTS_URL: {msg}"
        );
    }

    #[tokio::test]
    async fn disable_switch_short_circuits() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/cat"]"#);
        std::env::set_var("LOOM_DISABLE_TTS", "1");
        let err = synthesize("hello").await.unwrap_err();
        assert_eq!(err.kind(), "tts_disabled");
    }

    #[tokio::test]
    async fn malformed_cmd_is_config_invalid() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var("LOOM_TTS_CMD", "not json");
        assert_eq!(
            synthesize("hi").await.unwrap_err().kind(),
            "tts_config_invalid"
        );
        std::env::set_var("LOOM_TTS_CMD", "[]");
        assert_eq!(
            synthesize("hi").await.unwrap_err().kind(),
            "tts_config_invalid"
        );
    }

    #[tokio::test]
    async fn both_set_is_config_invalid() {
        // Fail loud on ambiguous config (plan-council P-A1) rather than silently
        // picking a winner.
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/cat"]"#);
        std::env::set_var("LOOM_TTS_URL", "https://example.com/tts");
        assert!(matches!(
            TtsBackend::from_env(),
            Err(TtsError::ConfigInvalid(_))
        ));
        assert_eq!(
            synthesize("hi").await.unwrap_err().kind(),
            "tts_config_invalid"
        );
    }

    #[tokio::test]
    async fn timeout_override_is_clamped() {
        let _g = ENV_LOCK.lock().await;
        let prev = std::env::var("LOOM_TTS_TIMEOUT_MS").ok();
        std::env::set_var("LOOM_TTS_TIMEOUT_MS", "999999999");
        assert_eq!(
            tts_timeout(),
            std::time::Duration::from_millis(TTS_TIMEOUT_MS_MAX)
        );
        std::env::set_var("LOOM_TTS_TIMEOUT_MS", "0");
        assert_eq!(
            tts_timeout(),
            std::time::Duration::from_millis(TTS_TIMEOUT_MS_MIN)
        );
        std::env::remove_var("LOOM_TTS_TIMEOUT_MS");
        assert_eq!(
            tts_timeout(),
            std::time::Duration::from_millis(TTS_TIMEOUT_MS_DEFAULT)
        );
        match prev {
            Some(v) => std::env::set_var("LOOM_TTS_TIMEOUT_MS", v),
            None => std::env::remove_var("LOOM_TTS_TIMEOUT_MS"),
        }
    }

    // ---- Cmd backend ----

    #[cfg(unix)] // hardcodes /bin/* paths; loom targets Unix (FND-0022)
    #[tokio::test]
    async fn cmd_happy_path_text_on_stdin() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        // /bin/cat echoes stdin → stdout, proving text reached stdin (not argv).
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/cat"]"#);
        let out = synthesize("the quick brown fox").await.unwrap();
        assert_eq!(out, b"the quick brown fox");
    }

    #[cfg(unix)] // hardcodes /bin/* paths; loom targets Unix (FND-0022)
    #[tokio::test]
    async fn cmd_never_uses_a_shell() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        // If this were run via `sh -c`, the arg would be command-substituted and
        // create `pwned`. With argv-exec it is a literal echoed arg.
        let dir = std::env::temp_dir().join(format!("loom_tts_pwn_{}", std::process::id()));
        let marker = dir.join("pwned");
        let _ = std::fs::remove_file(&marker);
        std::env::set_var(
            "LOOM_TTS_CMD",
            format!(r#"["/bin/echo","$(touch {})"]"#, marker.display()),
        );
        let _ = synthesize("hi").await; // output content irrelevant
        assert!(
            !marker.exists(),
            "shell expansion happened — argv-exec violated"
        );
    }

    #[cfg(unix)] // hardcodes /bin/* paths; loom targets Unix (FND-0022)
    #[tokio::test]
    async fn cmd_nonzero_exit_is_failed() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/false"]"#);
        assert_eq!(synthesize("hi").await.unwrap_err().kind(), "tts_failed");
    }

    #[cfg(unix)] // hardcodes /bin/* paths; loom targets Unix (FND-0022)
    #[tokio::test]
    async fn cmd_empty_output_is_failed() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/true"]"#);
        assert_eq!(synthesize("hi").await.unwrap_err().kind(), "tts_failed");
    }

    #[cfg(unix)] // hardcodes /bin/* paths; loom targets Unix (FND-0022)
    #[tokio::test]
    async fn cmd_over_cap_fails_fast_not_on_timeout() {
        // A producer that emits > MAX_INJECT_BYTES must be rejected as soon as the cap
        // is hit — NOT after the full timeout budget (ship-council FND-0003). ~9 MiB of
        // "y\n" exceeds the 8 MiB cap.
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var(
            "LOOM_TTS_CMD",
            r#"["/bin/sh","-c","yes | head -c 9000000"]"#,
        );
        let start = std::time::Instant::now();
        assert_eq!(synthesize("hi").await.unwrap_err().kind(), "tts_failed");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(4),
            "over-cap output waited for the timeout instead of failing fast"
        );
    }

    #[cfg(unix)] // hardcodes /bin/* paths; loom targets Unix (FND-0022)
    #[tokio::test]
    async fn cmd_timeout_sigkills_within_budget() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        // sh is the legitimately-configured program here (argv[0]); it sleeps past
        // the 5 s budget and must be SIGKILLed, returning promptly with tts_failed.
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/sh","-c","sleep 30"]"#);
        let start = std::time::Instant::now();
        let err = synthesize("hi").await.unwrap_err();
        assert_eq!(err.kind(), "tts_failed");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(15),
            "did not kill promptly"
        );
    }

    #[tokio::test]
    async fn text_over_cap_is_invalid_argument() {
        let _g = ENV_LOCK.lock().await;
        let _restore = EnvGuard::capture();
        EnvGuard::clear();
        std::env::set_var("LOOM_TTS_CMD", r#"["/bin/cat"]"#);
        let big = "a".repeat(MAX_TTS_TEXT_BYTES + 1);
        assert_eq!(
            synthesize(&big).await.unwrap_err().kind(),
            "invalid_argument"
        );
    }

    // ---- SSRF policy (pure predicates — production stays locked) ----

    #[test]
    fn ip_blocklist_covers_internal_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
        ] {
            assert!(
                ip_is_blocked(ip.parse::<IpAddr>().unwrap()),
                "{ip} must be blocked"
            );
        }
        // IPv4-mapped loopback must not slip past.
        assert!(ip_is_blocked(IpAddr::V6(
            "::ffff:127.0.0.1".parse::<Ipv6Addr>().unwrap()
        )));
        // A public address is allowed.
        assert!(!ip_is_blocked(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    }

    #[test]
    fn validate_rejects_http_and_private_ip() {
        // http scheme rejected even for a public IP.
        let pub_addr = vec![SocketAddr::from(([93, 184, 216, 34], 80))];
        assert!(validate_outbound_url("http://example.com/", &pub_addr).is_err());
        // https to a public IP accepted.
        let pub_https = vec![SocketAddr::from(([93, 184, 216, 34], 443))];
        assert!(validate_outbound_url("https://example.com/", &pub_https).is_ok());
        // https that resolved to a private IP rejected.
        let priv_addr = vec![SocketAddr::from(([127, 0, 0, 1], 443))];
        assert!(validate_outbound_url("https://example.com/", &priv_addr).is_err());
    }

    #[tokio::test]
    async fn resolve_and_validate_rejects_loopback_url() {
        // Real end-to-end policy check: a loopback URL is refused on the say path.
        let err = resolve_and_validate("https://127.0.0.1:9/tts")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "url_blocked");
        // And http is refused regardless of host.
        assert_eq!(
            resolve_and_validate("http://example.com/tts")
                .await
                .unwrap_err()
                .kind(),
            "url_blocked"
        );
    }

    // ---- Url transport (do_post against a loopback stub; policy already tested) ----

    #[tokio::test]
    async fn url_happy_path_returns_audio_bytes() {
        let addr = spawn_http_stub(vec![(200, "audio/wav", b"RIFFfake-wav".to_vec())]);
        let url = format!("http://{addr}/tts");
        let out = do_post(&url, &[addr], "hello").await.unwrap();
        assert_eq!(out, b"RIFFfake-wav");
    }

    #[tokio::test]
    async fn url_retries_on_5xx_then_succeeds() {
        let addr = spawn_http_stub(vec![
            (503, "text/plain", b"busy".to_vec()),
            (503, "text/plain", b"busy".to_vec()),
            (200, "audio/wav", b"RIFFok".to_vec()),
        ]);
        let url = format!("http://{addr}/tts");
        let out = do_post(&url, &[addr], "hello").await.unwrap();
        assert_eq!(out, b"RIFFok");
    }

    #[tokio::test]
    async fn url_gives_up_after_three_5xx() {
        let addr = spawn_http_stub(vec![
            (500, "text/plain", b"e".to_vec()),
            (500, "text/plain", b"e".to_vec()),
            (500, "text/plain", b"e".to_vec()),
        ]);
        let url = format!("http://{addr}/tts");
        assert_eq!(
            do_post(&url, &[addr], "hi").await.unwrap_err().kind(),
            "tts_failed"
        );
    }

    #[tokio::test]
    async fn url_does_not_retry_4xx() {
        // Only one canned response: a retry would hang on accept() and the 5 s
        // budget would elapse. Success here means we returned on the first 404.
        let addr = spawn_http_stub(vec![(404, "text/plain", b"nope".to_vec())]);
        let url = format!("http://{addr}/tts");
        let start = std::time::Instant::now();
        assert_eq!(
            do_post(&url, &[addr], "hi").await.unwrap_err().kind(),
            "tts_failed"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "retried a 4xx"
        );
    }

    #[tokio::test]
    async fn url_wrong_content_type_is_failed() {
        let addr = spawn_http_stub(vec![(200, "text/html", b"<html>error</html>".to_vec())]);
        let url = format!("http://{addr}/tts");
        assert_eq!(
            do_post(&url, &[addr], "hi").await.unwrap_err().kind(),
            "tts_failed"
        );
    }

    #[tokio::test]
    async fn url_refuses_redirects() {
        // A 3xx must NOT be followed — a redirect to an internal IP is the classic
        // SSRF-via-redirect bypass. Single canned response (no retry): a prompt
        // return proves we did not chase the Location.
        let addr = spawn_http_stub(vec![(302, "text/plain", b"moved".to_vec())]);
        let url = format!("http://{addr}/tts");
        let start = std::time::Instant::now();
        assert_eq!(
            do_post(&url, &[addr], "hi").await.unwrap_err().kind(),
            "tts_failed"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "followed a redirect instead of refusing it"
        );
    }
}
