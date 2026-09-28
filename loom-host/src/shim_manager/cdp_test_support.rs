//! Test-only helpers for reading `CdpMessage` params, shared by the pure
//! CDP-builder test modules (`input_dispatch`, `fill_prepare`).

use ciborium::value::Value;
use loom_shared::shim_protocol::CdpMessage;

pub(crate) fn method_of(m: &CdpMessage) -> &str {
    &m.method
}

pub(crate) fn field<'a>(m: &'a CdpMessage, k: &str) -> Option<&'a Value> {
    if let Value::Map(entries) = &m.params {
        entries.iter().find_map(|(kk, vv)| match kk {
            Value::Text(t) if t == k => Some(vv),
            _ => None,
        })
    } else {
        None
    }
}

pub(crate) fn text_of(v: &Value) -> Option<&str> {
    if let Value::Text(t) = v {
        Some(t)
    } else {
        None
    }
}

pub(crate) fn int_of(v: &Value) -> Option<i64> {
    if let Value::Integer(i) = v {
        Some((*i).try_into().ok()?)
    } else {
        None
    }
}
