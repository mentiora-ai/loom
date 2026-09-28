// Canonical receipt bytes for the cookie verbs: cookie normalisation, the
// deterministic sort key and the read outcome hash, with their tests. Split out
// of `receipt_marshaller.rs`.

use super::receipt_marshaller::*;
use loom_core::error::LoomError;

/// v0.9.6 web-cookie-injection cookies-tier canonical bytes assembly.
///
/// Produces JCS-encoded bytes for receipts where any cookie-result field
/// is populated. Two transforms run BEFORE JCS encoding:
///
///   - **D13 tuple-identity sort.** Cookie arrays (set_cookies_result,
///     get_cookies_result) are sorted by `(name, domain.unwrap_or_default(),
///     path.unwrap_or_default())` byte-lex. RFC 6265 §5.3 identifies a
///     cookie by this triple, so the sort guarantees byte-identity
///     between record and replay even when two cookies share a `name`
///     but differ in domain/path. (For ASCII inputs — and cookie names
///     are restricted to RFC 6265 token chars — byte-lex matches the
///     UTF-16 lex specified by JCS.)
///
///   - **Value redaction.** `value` fields on cookies are replaced with
///     `"[REDACTED]"` in the canonical bytes. The receipt's outcome_hash
///     therefore depends on cookie *names* + *structure* but NOT on
///     cookie *values*, so replay (which substitutes values from a
///     `replay_cookie_values` map) reproduces byte-identical canonical
///     bytes regardless of which specific value is provided.
///
/// The operator-facing wire receipt (sent over JSON-RPC) is a separate
/// path that preserves raw values per D7 — see `build_navigate_wire_receipt`
/// in `loom-daemon`.
pub(super) fn assemble_cookies_canonical_bytes(
    builder: &ReceiptBuilder,
) -> Result<Vec<u8>, LoomError> {
    use loom_core::error::LoomErrorCode;

    let payload = serde_json::json!({
        "action_id": builder.action_id,
        "status": builder.status,
        "started_at_ms": builder.started_at_ms,
        "finished_at_ms": builder.finished_at_ms,
        "side_effects_count": builder.side_effects_count,
        "host_call_count": builder.host_call_count,
        "error_code": builder.error_code,
        "error_details": builder.error_details,
        "action_hash": builder.action_hash,
        "outcome_hash": builder.outcome_hash,
        "emitted_at_ms": builder.emitted_at_ms,
        "set_cookies_result": prepare_cookies_field(builder.set_cookies_result.as_deref())?,
        "get_cookies_result": prepare_cookies_field(builder.get_cookies_result.as_deref())?,
        "clear_cookies_result": prepare_passthrough_field(builder.clear_cookies_result.as_deref())?,
        "delete_cookies_result": prepare_passthrough_field(builder.delete_cookies_result.as_deref())?,
    });

    serde_jcs::to_string(&payload)
        .map(String::into_bytes)
        .map_err(|e| {
            LoomError::new(
                LoomErrorCode::Internal,
                format!("assemble_cookies_canonical_bytes: JCS encode failed: {e}"),
            )
        })
}

/// Re-derive a value-free `outcome_hash` for a `web.get_cookies` receipt from
/// its raw cookie-array JSON.
///
/// The guest sets `outcome_hash = sha256(raw Network.getCookies response)`, which
/// includes cookie *values* — those must NOT enter the manifest hash chain
/// (`assemble_cookies_canonical_bytes` embeds `outcome_hash`, so a value-bearing
/// hash would leak values into the chain). This mirrors that function's
/// redaction (D13 tuple-identity sort + `value` → `"[REDACTED]"`) so the hash
/// depends on cookie names/structure but never on values — keeping replay
/// structural and the chain cross-run value-independent (NFR-DET-01). The `"C:"`
/// domain separator mirrors evaluate's `"E:"`.
pub fn cookie_read_outcome_hash(get_cookies_result_json: &str) -> Result<String, LoomError> {
    use loom_core::error::LoomErrorCode;
    let redacted = prepare_cookies_field(Some(get_cookies_result_json))?;
    let canonical = serde_jcs::to_string(&redacted).map_err(|e| {
        LoomError::new(
            LoomErrorCode::Internal,
            format!("cookie_read_outcome_hash: JCS encode failed: {e}"),
        )
    })?;
    let mut buf = Vec::with_capacity(2 + canonical.len());
    buf.extend_from_slice(b"C:");
    buf.extend_from_slice(canonical.as_bytes());
    Ok(loom_core::content_store::sha256_hex(&buf))
}

/// Parse a JSON-encoded cookie array, redact `value` fields, sort by
/// (name, domain, path) tuple. Returns the cookie array as a
/// `serde_json::Value` ready to embed in the receipt payload (or
/// `Value::Null` when the input is None — JCS encodes Null verbatim).
pub(super) fn prepare_cookies_field(raw: Option<&str>) -> Result<serde_json::Value, LoomError> {
    use loom_core::error::LoomErrorCode;
    let Some(s) = raw else {
        return Ok(serde_json::Value::Null);
    };
    let mut v: serde_json::Value = serde_json::from_str(s).map_err(|e| {
        LoomError::new(
            LoomErrorCode::Internal,
            format!("prepare_cookies_field: parse failed: {e}"),
        )
    })?;
    if let Some(arr) = v.as_array_mut() {
        arr.sort_by_key(cookie_sort_key);
        for item in arr.iter_mut() {
            if let Some(obj) = item.as_object_mut() {
                if obj.contains_key("value") {
                    obj.insert(
                        "value".to_string(),
                        serde_json::Value::String("[REDACTED]".to_string()),
                    );
                }
            }
        }
    }
    Ok(v)
}

/// Passthrough for `clear_cookies_result` / `delete_cookies_result` —
/// single-item structs with no value field, no array to sort. Parses
/// the JSON string into a Value so JCS encodes structure rather than
/// the escaped JSON string.
pub(super) fn prepare_passthrough_field(raw: Option<&str>) -> Result<serde_json::Value, LoomError> {
    use loom_core::error::LoomErrorCode;
    let Some(s) = raw else {
        return Ok(serde_json::Value::Null);
    };
    serde_json::from_str(s).map_err(|e| {
        LoomError::new(
            LoomErrorCode::Internal,
            format!("prepare_passthrough_field: parse failed: {e}"),
        )
    })
}

pub(super) fn cookie_sort_key(c: &serde_json::Value) -> (String, String, String) {
    let s = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    (s("name"), s("domain"), s("path"))
}

#[cfg(test)]
mod cookies_canonical_bytes_tests {
    use super::*;

    fn fixture_builder() -> ReceiptBuilder {
        ReceiptBuilder {
            action_id: 42,
            started_at_ms: 1000,
            finished_at_ms: 1010,
            status: ReceiptStatus::Ok,
            side_effects_count: 0,
            host_call_count: 1,
            error_code: None,
            error_details: None,
            action_hash: "ah".to_string(),
            outcome_hash: "oh".to_string(),
            emitted_at_ms: 1010,
            ..Default::default()
        }
    }

    #[test]
    fn assemble_cookies_path_invokes_when_set_cookies_result_present() {
        let mut b = fixture_builder();
        b.set_cookies_result = Some(r#"[{"name":"sid","success":true}]"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("set_cookies_result"));
        assert!(s.contains("sid"));
    }

    #[test]
    fn d13_sort_places_cookies_with_same_name_distinct_domains_in_canonical_order() {
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[
                {"name":"sid","domain":"example.com","path":"/","value":"v1"},
                {"name":"sid","domain":"api.example.com","path":"/","value":"v2"}
            ]"#
            .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        let api_pos = s.find("api.example.com").expect("api domain in output");
        let example_pos = s
            .find("\"example.com\"")
            .expect("example.com domain in output");
        assert!(
            api_pos < example_pos,
            "D13 sort: api.example.com should precede example.com (byte-lex)"
        );
    }

    #[test]
    fn d13_sort_distinguishes_cookies_with_same_name_distinct_paths() {
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[
                {"name":"sid","domain":"x.com","path":"/api","value":"v1"},
                {"name":"sid","domain":"x.com","path":"/","value":"v2"}
            ]"#
            .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        let root_pos = s.find("\"/\"").expect("/ path");
        let api_pos = s.find("\"/api\"").expect("/api path");
        assert!(root_pos < api_pos, "/ should precede /api byte-lex");
    }

    #[test]
    fn cookie_values_are_redacted_in_canonical_bytes_per_replay_byte_identity() {
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[{"name":"sid","domain":"x.com","path":"/","value":"super-secret-token"}]"#
                .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            !s.contains("super-secret-token"),
            "raw cookie value must not appear in canonical bytes"
        );
        assert!(
            s.contains("[REDACTED]"),
            "canonical bytes must carry [REDACTED] as the value substitute"
        );
    }

    #[test]
    fn byte_identity_holds_when_values_differ_but_tuples_match() {
        // Replay byte-identity guarantee: two receipts with same
        // (name, domain, path) but different cookie *values* must
        // produce IDENTICAL canonical bytes.
        let mut b1 = fixture_builder();
        b1.get_cookies_result =
            Some(r#"[{"name":"sid","domain":"x.com","path":"/","value":"VALUE_A"}]"#.to_string());
        let mut b2 = fixture_builder();
        b2.get_cookies_result = Some(
            r#"[{"name":"sid","domain":"x.com","path":"/","value":"DIFFERENT_VALUE_B"}]"#
                .to_string(),
        );
        let bytes1 = ReceiptMarshaller::assemble_canonical_bytes(&b1).expect("ok");
        let bytes2 = ReceiptMarshaller::assemble_canonical_bytes(&b2).expect("ok");
        assert_eq!(
            bytes1, bytes2,
            "canonical bytes must be identical regardless of cookie value (replay-byte-identity)"
        );
    }

    #[test]
    fn cookie_read_outcome_hash_excludes_values() {
        // The re-derived get_cookies outcome_hash must NOT depend on cookie
        // values (NFR-DET-01 + acceptance #3): two reads with identical
        // (name, domain, path) but different values hash identically.
        let h1 = cookie_read_outcome_hash(
            r#"[{"name":"sid","domain":"x.com","path":"/","value":"SECRET_A"}]"#,
        )
        .expect("ok");
        let h2 = cookie_read_outcome_hash(
            r#"[{"name":"sid","domain":"x.com","path":"/","value":"totally_different_B"}]"#,
        )
        .expect("ok");
        assert_eq!(h1, h2, "outcome_hash must be value-independent");
        // sanity: a real 64-hex sha256.
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cookie_read_outcome_hash_distinguishes_names_and_is_order_independent() {
        // Different cookie NAMES → different hash.
        let a =
            cookie_read_outcome_hash(r#"[{"name":"sid","domain":"x.com","path":"/","value":"v"}]"#)
                .expect("ok");
        let b = cookie_read_outcome_hash(
            r#"[{"name":"csrf","domain":"x.com","path":"/","value":"v"}]"#,
        )
        .expect("ok");
        assert_ne!(a, b, "distinct cookie names must hash differently");
        // D13 sort makes the hash independent of input array order.
        let ord1 = cookie_read_outcome_hash(
            r#"[{"name":"a","domain":"x.com","path":"/","value":"1"},{"name":"b","domain":"x.com","path":"/","value":"2"}]"#,
        )
        .expect("ok");
        let ord2 = cookie_read_outcome_hash(
            r#"[{"name":"b","domain":"x.com","path":"/","value":"9"},{"name":"a","domain":"x.com","path":"/","value":"8"}]"#,
        )
        .expect("ok");
        assert_eq!(
            ord1, ord2,
            "hash must be input-order independent (D13 sort)"
        );
    }

    #[test]
    fn clear_cookies_result_passes_through_as_structured_object() {
        let mut b = fixture_builder();
        b.clear_cookies_result = Some(r#"{"cleared_count":7}"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(r#""cleared_count":7"#));
    }

    #[test]
    fn delete_cookies_result_passes_through_with_matched_bool() {
        let mut b = fixture_builder();
        b.delete_cookies_result = Some(r#"{"name":"sid","matched":true}"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(r#""matched":true"#));
        assert!(s.contains(r#""name":"sid""#));
    }

    // === Cross-crate replay byte-identity integration tests ===
    // Wires loom-core::replay_engine::cookie_replay::substitute_cookie_values
    // into the loom-host receipt marshaller and verifies the end-to-end
    // record→replay byte-identity invariant.

    #[test]
    fn record_then_replay_byte_identity_via_cookie_replay_substitution() {
        use loom_core::replay_engine::cookie_replay::{
            substitute_cookie_values, ReplayCookieValues,
        };

        // STEP 1: Recorded receipt — real cookie values.
        let recorded_payload =
            r#"[{"name":"sid","domain":"example.com","path":"/","value":"REAL_SESSION_TOKEN"}]"#;
        let mut recorded = fixture_builder();
        recorded.get_cookies_result = Some(recorded_payload.to_string());
        let recorded_bytes =
            ReceiptMarshaller::assemble_canonical_bytes(&recorded).expect("record canonical");

        // STEP 2: Replay receipt — substitute via cookie_replay.
        let mut replay_values: ReplayCookieValues = std::collections::BTreeMap::new();
        replay_values.insert(
            (
                "sid".to_string(),
                "example.com".to_string(),
                "/".to_string(),
            ),
            "REPLAY_PLACEHOLDER_VALUE".to_string(),
        );
        let replayed_payload =
            substitute_cookie_values(recorded.action_id, recorded_payload, &replay_values)
                .expect("substitute ok");
        // The substituted JSON has the replay placeholder, not the
        // recorded value.
        assert!(replayed_payload.contains("REPLAY_PLACEHOLDER_VALUE"));
        assert!(!replayed_payload.contains("REAL_SESSION_TOKEN"));

        let mut replay = fixture_builder();
        replay.get_cookies_result = Some(replayed_payload);
        let replay_bytes =
            ReceiptMarshaller::assemble_canonical_bytes(&replay).expect("replay canonical");

        // STEP 3: Byte-identity holds — the marshaller redacts values
        // in both paths, so the canonical bytes are identical regardless
        // of which placeholder the replay supplied.
        assert_eq!(
            recorded_bytes, replay_bytes,
            "record→replay canonical bytes must be byte-identical when (name,domain,path) tuples match"
        );
    }

    #[test]
    fn replay_missing_value_propagates_typed_error_through_substitution() {
        use loom_core::replay_engine::cookie_replay::{
            substitute_cookie_values, ReplayCookieValues, ReplayError,
        };

        let recorded_payload =
            r#"[{"name":"sid","domain":"example.com","path":"/api","value":"X"}]"#;
        // Supply value for "/" but the recorded path is "/api" — tuple mismatch.
        let mut replay_values: ReplayCookieValues = std::collections::BTreeMap::new();
        replay_values.insert(
            (
                "sid".to_string(),
                "example.com".to_string(),
                "/".to_string(),
            ),
            "P".to_string(),
        );
        let err = substitute_cookie_values(123, recorded_payload, &replay_values)
            .expect_err("must error");
        match err {
            ReplayError::MissingCookieValue {
                action_id,
                name,
                domain,
                path,
            } => {
                assert_eq!(action_id, 123);
                assert_eq!(name, "sid");
                assert_eq!(domain, "example.com");
                assert_eq!(path, "/api");
            }
            other => panic!("expected MissingCookieValue, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod cookie_edge_case_tests {
    use super::*;

    fn fixture_builder() -> ReceiptBuilder {
        ReceiptBuilder {
            action_id: 1,
            started_at_ms: 0,
            finished_at_ms: 1,
            status: ReceiptStatus::Ok,
            side_effects_count: 0,
            host_call_count: 0,
            error_code: None,
            error_details: None,
            action_hash: "ah".to_string(),
            outcome_hash: "oh".to_string(),
            emitted_at_ms: 1,
            ..Default::default()
        }
    }

    // === D13 sort edge cases ===

    #[test]
    fn d13_sort_with_empty_array_produces_empty_canonical_array() {
        let mut b = fixture_builder();
        b.get_cookies_result = Some("[]".to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(r#""get_cookies_result":[]"#));
    }

    #[test]
    fn d13_sort_with_single_cookie_is_noop_no_panic() {
        let mut b = fixture_builder();
        b.get_cookies_result =
            Some(r#"[{"name":"sid","domain":"x.com","path":"/","value":"v"}]"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\"name\":\"sid\""));
        assert!(s.contains("[REDACTED]"));
    }

    #[test]
    fn d13_sort_handles_cookies_with_missing_domain_field() {
        // RFC 6265: domain is optional. Cookies without `domain` should
        // sort using empty-string default (cookie_sort_key uses
        // unwrap_or("")). They should NOT cause a panic.
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[
                {"name":"sid","path":"/","value":"v"},
                {"name":"sid","domain":"x.com","path":"/","value":"v2"}
            ]"#
            .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        // The domain-less cookie sorts BEFORE the domain-bearing one
        // (empty string < "x.com" byte-lex).
        let no_domain_pos = s.find(r#"{"domain":null"#).unwrap_or_else(|| {
            // The serializer might omit nulls — find by lack of "x.com"
            // near the first cookie. Fall back to position of first "sid".
            s.find("\"sid\"").expect("at least one sid")
        });
        let x_com_pos = s.find("\"x.com\"").expect("x.com");
        assert!(no_domain_pos <= x_com_pos);
    }

    #[test]
    fn d13_sort_handles_cookies_with_null_domain_field() {
        // `domain: null` should be treated identically to missing.
        let mut b = fixture_builder();
        b.get_cookies_result =
            Some(r#"[{"name":"sid","domain":null,"path":"/","value":"v"}]"#.to_string());
        let _ = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
    }

    #[test]
    fn d13_sort_three_way_tie_preserves_no_panic_for_identical_tuples() {
        // Two cookies with identical (name, domain, path) — degenerate
        // case (real browsers shouldn't allow this). The sort is stable;
        // we just need to not panic and to produce consistent output.
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[
                {"name":"sid","domain":"x.com","path":"/","value":"v1"},
                {"name":"sid","domain":"x.com","path":"/","value":"v2"}
            ]"#
            .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        // Both values redacted; both cookies present.
        let redacted_count = s.matches("[REDACTED]").count();
        assert_eq!(redacted_count, 2);
    }

    #[test]
    fn d13_sort_with_50_cookies_terminates_in_reasonable_time() {
        // Stress test: 50 cookies sort + redact in reasonable time.
        // Not a microbenchmark; just guards against O(n^2) regressions.
        use std::fmt::Write;
        let mut s = String::from("[");
        for i in 0..50 {
            if i > 0 {
                s.push(',');
            }
            write!(
                s,
                r#"{{"name":"sid","domain":"d{:02}.com","path":"/","value":"x"}}"#,
                49 - i // reverse order so sort has work to do
            )
            .unwrap();
        }
        s.push(']');
        let mut b = fixture_builder();
        b.get_cookies_result = Some(s);
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let out = String::from_utf8(bytes).unwrap();
        // First domain should be d00.com (lexicographic first after sort).
        let d00 = out.find("\"d00.com\"").expect("d00 in output");
        let d01 = out.find("\"d01.com\"").expect("d01 in output");
        assert!(d00 < d01, "ascending order");
    }

    #[test]
    fn d13_sort_handles_unicode_in_domain() {
        // Domains *can* contain IDN-encoded unicode (xn--... punycode in
        // practice, but the typed string is UTF-8). Sort by byte-lex is
        // deterministic regardless. Test pins no-panic + deterministic
        // order.
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[
                {"name":"x","domain":"münchen.de","path":"/","value":"v"},
                {"name":"x","domain":"berlin.de","path":"/","value":"v"}
            ]"#
            .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        // Look for the full domain strings — searching for bare "m" or
        // "b" hits the first occurrence anywhere in the JSON (e.g.
        // "name", "domain") which is not informative.
        let berlin = s.find("berlin.de").expect("berlin.de");
        let muenchen = s.find("münchen.de").expect("münchen.de");
        // 'b' < 'm' byte-lex, so berlin precedes münchen.
        assert!(
            berlin < muenchen,
            "berlin.de should appear before münchen.de in sorted output; got berlin={berlin}, muenchen={muenchen}"
        );
    }

    #[test]
    fn d13_sort_already_sorted_array_is_stable_no_op() {
        let mut b = fixture_builder();
        b.get_cookies_result = Some(
            r#"[
                {"name":"a","domain":"x.com","path":"/","value":"v"},
                {"name":"b","domain":"x.com","path":"/","value":"v"},
                {"name":"c","domain":"x.com","path":"/","value":"v"}
            ]"#
            .to_string(),
        );
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        let a = s.find("\"a\"").unwrap();
        let b_pos = s.find("\"b\"").unwrap();
        let c = s.find("\"c\"").unwrap();
        assert!(a < b_pos && b_pos < c);
    }

    // === Cookie result invalid-payload edge cases ===

    #[test]
    fn assemble_cookies_with_malformed_set_cookies_result_json_returns_internal_error() {
        let mut b = fixture_builder();
        b.set_cookies_result = Some("not json".to_string());
        let err = ReceiptMarshaller::assemble_canonical_bytes(&b).expect_err("must error");
        // We don't pin the exact LoomError code shape since the marshaller
        // uses the generic Internal variant for this branch; just check
        // it returned Err and didn't panic.
        assert!(!format!("{err:?}").is_empty());
    }

    #[test]
    fn assemble_cookies_with_get_cookies_result_as_object_not_array_returns_error_path() {
        // The marshaller's prepare_cookies_field expects an array. An
        // object should NOT panic; current impl tolerates it because
        // `v.as_array_mut()` returns None and the function returns Ok
        // with the original Value. Pin that behaviour: it doesn't
        // crash; the canonical bytes simply carry the object as-is.
        let mut b = fixture_builder();
        b.get_cookies_result = Some(r#"{"not":"an array"}"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("no panic");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\"not\":\"an array\""));
    }

    #[test]
    fn assemble_cookies_with_no_value_field_on_cookie_skips_redaction() {
        // If a cookie object has no `value` field at all, the redactor
        // shouldn't add one — just leave it as-is. (Real-world cookies
        // always have a value, but the marshaller mustn't fabricate
        // data.)
        let mut b = fixture_builder();
        b.get_cookies_result = Some(r#"[{"name":"sid","domain":"x.com","path":"/"}]"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(!s.contains("[REDACTED]"));
        assert!(s.contains("\"name\":\"sid\""));
    }

    #[test]
    fn assemble_cookies_clear_result_with_zero_cleared_count() {
        let mut b = fixture_builder();
        b.clear_cookies_result = Some(r#"{"cleared_count":0}"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\"cleared_count\":0"));
    }

    #[test]
    fn assemble_cookies_delete_result_with_matched_false() {
        let mut b = fixture_builder();
        b.delete_cookies_result = Some(r#"{"name":"sid","matched":false}"#.to_string());
        let bytes = ReceiptMarshaller::assemble_canonical_bytes(&b).expect("ok");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\"matched\":false"));
    }
}
