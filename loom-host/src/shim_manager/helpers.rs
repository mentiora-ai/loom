// ShimManager — free-function helpers.
//
// Pure / side-effecting helpers shared by the manager core and the per-verb
// senders: wall-clock, session-id validation + profile-dir cleanup, shim error
// code/class mapping, and CBOR extraction + `Runtime.evaluate` payload parsing.
// Split out of `shim_manager.rs` (behavior-preserving); all `pub(crate)` so the
// sibling submodules can call them unchanged.

use super::types::{EvaluateException, EvaluateOutcome, FailureClass};
use loom_core::error::{LoomError, LoomErrorCode};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Validate a session id against the canonical safe charset before it is used
/// to build a filesystem path or a process-match pattern. Guards against path
/// traversal (`../`) in the profile-dir `remove_dir_all` and against injection
/// into the watcher's `pkill -f user-data-dir=...` pattern. Session ids are
/// daemon-generated, but this is defense-in-depth: a malformed id is refused,
/// never acted on.
pub(crate) fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The per-session chromium profile dir, mirroring the path the host exports as
/// `LOOM_SHIM_USER_DATA_DIR` (`<tmp>/loom-chromium-<session_id>`). `None` if the
/// id fails validation (caller skips cleanup rather than touching an unsafe path).
pub(crate) fn session_profile_dir(session_id: &str) -> Option<PathBuf> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    Some(std::env::temp_dir().join(format!("loom-chromium-{session_id}")))
}

/// Remove the per-session chromium profile dir on session close. Idempotent:
/// a missing dir (already reaped, or never created for a session that never
/// navigated) is success; other errors are logged, not propagated.
pub(crate) fn remove_session_profile_dir(session_id: &str) {
    let Some(dir) = session_profile_dir(session_id) else {
        tracing::warn!(
            session = %session_id,
            "refusing to clean profile dir for unsafe session id"
        );
        return;
    };
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => tracing::debug!(dir = %dir.display(), "removed session profile dir"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "profile dir cleanup failed"),
    }
}

pub(crate) fn map_shim_code(code: loom_shared::shim_protocol::ShimErrorCode) -> LoomErrorCode {
    use loom_shared::shim_protocol::ShimErrorCode as E;
    match code {
        E::ChromiumUnavailable => LoomErrorCode::ShimFailure,
        E::CdpTimeout => LoomErrorCode::ShimTimeout,
        E::CdpProtocolError | E::TargetUnknown | E::ShimInternalError => LoomErrorCode::ShimFailure,
    }
}

/// Classify a shim-REPORTED error envelope for `record_failure`.
/// `ChromiumUnavailable` means the shim's browser is gone and its own
/// restart budget is exhausted — the supervisor contract hands the
/// respawn decision to the ShimManager, so it is transport class (evict;
/// the next admitted call respawns the whole shim). Everything else came
/// from a live shim with a running Chromium (bad CDP params, unknown
/// target, slow page, internal shim error) — application class: count
/// toward the breaker, keep the browser alive.
pub(crate) fn shim_error_class(code: &loom_shared::shim_protocol::ShimErrorCode) -> FailureClass {
    use loom_shared::shim_protocol::ShimErrorCode as E;
    match code {
        E::ChromiumUnavailable => FailureClass::Transport,
        E::CdpTimeout | E::CdpProtocolError | E::TargetUnknown | E::ShimInternalError => {
            FailureClass::Application
        }
    }
}

/// Classify a HOST-side round-trip error (the `Err` arm of `send_and_await` and
/// the helpers built on it) for `record_failure`.
///
/// The counterpart to [`shim_error_class`], which classifies errors the shim
/// REPORTED. Both answer the same question: did this failure come from a live,
/// responsive shim? If it did, the browser must stay up.
///
/// A **recv** timeout is `Application`: the frame reached the shim, which is
/// alive and working — the PAGE was slow. Note `shim_error_class` already treats
/// the shim's own `CdpTimeout` ("slow page") as `Application`; a host-side recv
/// timeout on that very same slow page is the same condition observed by a
/// different clock, so it must land in the same class. Classing it `Transport`
/// evicted the subprocess and killed the session's healthy Chromium, losing the
/// whole session over one slow screenshot (#347).
///
/// Everything else stays `Transport`: a send timeout (the frame never left the
/// host), a closed channel, a dropped oneshot, a crashed subprocess, framing and
/// demux violations. Those mean the subprocess or its socket is genuinely
/// unhealthy and the next admitted call must respawn.
pub(crate) fn host_call_class(err: &LoomError) -> FailureClass {
    let is_recv_timeout = err.code == LoomErrorCode::ShimTimeout
        && err
            .context
            .as_ref()
            .and_then(|c| c.get(super::process::TIMEOUT_PHASE_KEY))
            .and_then(|v| v.as_str())
            == Some(super::process::TIMEOUT_PHASE_RECV);
    if is_recv_timeout {
        FailureClass::Application
    } else {
        FailureClass::Transport
    }
}

/// Fetch a string-keyed field from a CBOR map `Value`.
pub(crate) fn cbor_get<'a>(
    v: &'a ciborium::value::Value,
    key: &str,
) -> Option<&'a ciborium::value::Value> {
    if let ciborium::value::Value::Map(entries) = v {
        for (k, val) in entries {
            if let ciborium::value::Value::Text(t) = k {
                if t == key {
                    return Some(val);
                }
            }
        }
    }
    None
}

/// Interpret a CBOR `Value` as a non-negative `u64` (CDP nodeIds).
pub(crate) fn cbor_u64(v: &ciborium::value::Value) -> Option<u64> {
    if let ciborium::value::Value::Integer(i) = v {
        u64::try_from(i128::from(*i)).ok()
    } else {
        None
    }
}

/// Parse a CDP `Runtime.evaluate` response payload (CBOR map) into an
/// `EvaluateOutcome`. The response shape is documented at
/// https://chromedevtools.github.io/devtools-protocol/tot/Runtime/#method-evaluate
pub(crate) fn parse_evaluate_payload(
    payload: &ciborium::value::Value,
) -> Result<EvaluateOutcome, String> {
    use ciborium::value::Value;

    let map = match payload {
        Value::Map(m) => m,
        other => return Err(format!("expected CBOR map, got {other:?}")),
    };

    let lookup = |key: &str| -> Option<&Value> {
        map.iter().find_map(|(k, v)| {
            if let Value::Text(s) = k {
                if s == key {
                    Some(v)
                } else {
                    None
                }
            } else {
                None
            }
        })
    };

    if let Some(ed) = lookup("exceptionDetails") {
        let ed_map = match ed {
            Value::Map(m) => m,
            _ => return Err("exceptionDetails not a map".into()),
        };
        let ed_lookup = |key: &str| -> Option<&Value> {
            ed_map.iter().find_map(|(k, v)| {
                if let Value::Text(s) = k {
                    if s == key {
                        Some(v)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        };
        let text = match ed_lookup("text") {
            Some(Value::Text(s)) => s.clone(),
            _ => "Uncaught".into(),
        };
        let line = match ed_lookup("lineNumber") {
            Some(Value::Integer(i)) => u32::try_from(i128::from(*i)).unwrap_or(0),
            _ => 0,
        };
        let column = match ed_lookup("columnNumber") {
            Some(Value::Integer(i)) => u32::try_from(i128::from(*i)).unwrap_or(0),
            _ => 0,
        };
        // exception is a RemoteObject — pull description (preferred) or
        // value (fallback for primitive throws). Per CDP, `description`
        // carries the human-readable string for Error objects;
        // `value` carries the stringified primitive for `throw "x"`.
        let message = match ed_lookup("exception") {
            Some(Value::Map(em)) => {
                let em_lookup = |key: &str| -> Option<&Value> {
                    em.iter().find_map(|(k, v)| {
                        if let Value::Text(s) = k {
                            if s == key {
                                Some(v)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                };
                if let Some(Value::Text(s)) = em_lookup("description") {
                    s.clone()
                } else if let Some(v) = em_lookup("value") {
                    stringify_cbor_primitive(v)
                } else {
                    text.clone()
                }
            }
            _ => text.clone(),
        };
        return Ok(EvaluateOutcome {
            result: None,
            exception: Some(EvaluateException {
                text,
                message,
                line,
                column,
            }),
        });
    }

    // Success path: result.value (or result.unserializableValue for things
    // like Infinity / NaN that don't survive CBOR. CDP places them in
    // `unserializableValue` as strings.)
    let result_obj = lookup("result")
        .ok_or_else(|| "evaluate response missing both result and exceptionDetails".to_string())?;
    let result_map = match result_obj {
        Value::Map(m) => m,
        _ => return Err("result not a map".into()),
    };
    let res_lookup = |key: &str| -> Option<&Value> {
        result_map.iter().find_map(|(k, v)| {
            if let Value::Text(s) = k {
                if s == key {
                    Some(v)
                } else {
                    None
                }
            } else {
                None
            }
        })
    };
    if let Some(v) = res_lookup("value") {
        return Ok(EvaluateOutcome {
            result: Some(v.clone()),
            exception: None,
        });
    }
    if let Some(Value::Text(s)) = res_lookup("unserializableValue") {
        // NaN, Infinity, -Infinity, -0, BigInt → CDP serializes as a
        // string. Surface as a Text value so cbor_value_to_json can
        // string-coerce per Q6.
        return Ok(EvaluateOutcome {
            result: Some(Value::Text(s.clone())),
            exception: None,
        });
    }
    // `evaluate('undefined')` returns { result: { type: "undefined" } }
    // with no `value`. Surface as Null per CDP convention.
    Ok(EvaluateOutcome {
        result: Some(Value::Null),
        exception: None,
    })
}

/// Stringify a primitive CBOR value for inclusion in an exception message.
pub(crate) fn stringify_cbor_primitive(v: &ciborium::value::Value) -> String {
    use ciborium::value::Value;
    match v {
        Value::Text(s) => s.clone(),
        Value::Integer(i) => format!("{}", i128::from(*i)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        other => format!("{other:?}"),
    }
}
