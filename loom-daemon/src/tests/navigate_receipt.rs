use super::*;

///..04: default profile carries every brief-listed key
/// when the upstream JSON blobs are well-formed. Tests the actual
/// production decode path.
#[test]
fn build_navigate_wire_receipt_decodes_all_three_json_blobs_under_default() {
    let builder = navigate_builder_with_all_blobs();
    let r = build_navigate_wire_receipt(&builder, "S1", None);

    assert_eq!(r.action_id, 11);
    assert_eq!(r.session_id, "S1");
    assert_eq!(r.url.as_deref(), Some("https://example.com/"));
    assert_eq!(r.status_code, Some(200));
    assert_eq!(r.dom_snapshot_hash.as_ref().map(String::len), Some(64));
    assert_eq!(r.screenshot_after_hash.as_ref().map(String::len), Some(64));
    assert_eq!(r.console_lines.len(), 1);
    assert_eq!(r.console_lines[0].level, "info");
    let s = r.network_summary.as_ref().expect("network_summary present");
    assert_eq!(s.total_count, 2);
    assert_eq!(s.total_bytes, 5120);
    assert_eq!(s.error_count, 0);
    assert_eq!(r.side_effects.len(), 2);
    assert_eq!(r.side_effects[0]["method"], "GET");
    assert_eq!(r.side_effects[0]["status"], 200);
}

/// web.get_cookies surfacing: a builder carrying `get_cookies_result`
/// (set host-side from the decoded Network.getCookies response) must land
/// on the wire receipt as a parsed JSON array with RAW values (D7).
#[test]
fn build_navigate_wire_receipt_surfaces_get_cookies_result() {
    let builder = ReceiptBuilder {
        action_id: 7,
        status: HostStatus::Ok,
        action_hash: "aa".repeat(32),
        outcome_hash: "cc".repeat(32),
        emitted_at_ms: 1_714_074_336_000,
        get_cookies_result: Some(
            r#"[{"name":"NID","value":"raw-token","domain":".google.com","httpOnly":true}]"#
                .to_string(),
        ),
        ..Default::default()
    };
    let r = build_navigate_wire_receipt(&builder, "S1", None);
    let cookies = r.get_cookies_result.expect("get_cookies_result surfaced");
    assert!(cookies.is_array());
    assert_eq!(cookies[0]["name"], "NID");
    // RAW value preserved on the operator-facing wire receipt (D7).
    assert_eq!(cookies[0]["value"], "raw-token");
    assert_eq!(cookies[0]["httpOnly"], true);
}

/// Non-cookie verbs leave `get_cookies_result` absent (no regression).
#[test]
fn build_navigate_wire_receipt_omits_get_cookies_for_non_cookie_verbs() {
    let builder = navigate_builder_with_all_blobs();
    let r = build_navigate_wire_receipt(&builder, "S1", None);
    assert!(r.get_cookies_result.is_none());
}

/// --capture-policy minimal strips tier-2 fields
/// at the wire boundary. This is the test that actually exercises
/// the `apply_capture_profile_to_wire(...)` invocation in the
/// production code path.
#[test]
fn build_navigate_wire_receipt_minimal_strips_per_brief() {
    let builder = navigate_builder_with_all_blobs();
    let r = build_navigate_wire_receipt(&builder, "S1", Some("minimal"));

    // Identity + brief-listed survivors:
    assert_eq!(r.action_id, 11);
    assert_eq!(r.session_id, "S1");
    assert_eq!(r.url.as_deref(), Some("https://example.com/"));
    assert_eq!(r.status_code, Some(200));

    // Stripped:
    assert!(r.dom_snapshot_hash.is_none());
    assert!(r.screenshot_after_hash.is_none());
    assert!(r.console_lines.is_empty());
    assert!(r.network_summary.is_none());
    assert!(r.network_count.is_none());
    assert!(r.console_count.is_none());
    assert!(r.final_url.is_none());
    assert!(r.title.is_none());
    assert!(r.side_effects.is_empty());
    assert!(r.action_hash.is_none());
    assert!(r.outcome_hash.is_none());
    assert!(r.emitted_at_ms.is_none());
}

/// network_entries side-channel surfaces inline AND leaves the
/// existing hashed/aggregate fields (network_count, side_effects,
/// network_summary) byte-identical (backward-compat: separate path).
#[test]
fn build_navigate_wire_receipt_surfaces_inline_network_entries() {
    let mut builder = navigate_builder_with_all_blobs();
    let entries = vec![loom_shared::navigate_outcome::LoomNetworkEntry {
        url: "https://app.test/api/thing".into(),
        method: "GET".into(),
        status: 200,
        resource_type: "XHR".into(),
        from_cache: false,
        request_id: "R-1".into(),
        ts_ms: 1_700_000_000_000,
    }];
    builder.navigate_network_entries_json = Some(serde_json::to_vec(&entries).unwrap());
    let r = build_navigate_wire_receipt(&builder, "S1", None);

    // network_entries surfaced.
    assert_eq!(r.network_entries.len(), 1);
    assert_eq!(r.network_entries[0]["method"], "GET");
    assert_eq!(r.network_entries[0]["status"], 200);
    assert_eq!(r.network_entries[0]["resource_type"], "XHR");
    assert!(r.network_entries_blob_ref.is_none());

    // Backward-compat: the existing fields are untouched by the new path.
    assert_eq!(r.network_count, Some(2));
    assert_eq!(r.side_effects.len(), 2);
    assert!(r.network_summary.is_some());
}

/// When the host offloaded the list, the wire receipt carries the
/// blob_ref (sha256) and an EMPTY inline list — the inline-XOR-blob
/// discriminator, mirroring return_value_blob_ref.
#[test]
fn build_navigate_wire_receipt_surfaces_network_entries_blob_ref() {
    let mut builder = navigate_builder_with_all_blobs();
    builder.navigate_network_entries_json = None;
    builder.navigate_network_entries_blob_ref = Some(loom_core::content_store::ContentRef {
        sha256: "c".repeat(64),
        size_bytes: 70_000,
    });
    builder.navigate_network_entries_truncated = Some(false);
    let r = build_navigate_wire_receipt(&builder, "S1", None);

    assert!(r.network_entries.is_empty());
    assert_eq!(
        r.network_entries_blob_ref.as_ref().map(String::len),
        Some(64)
    );
}

/// --capture-policy minimal strips the observational network_entries.
#[test]
fn build_navigate_wire_receipt_minimal_strips_network_entries() {
    let mut builder = navigate_builder_with_all_blobs();
    let entries = vec![loom_shared::navigate_outcome::LoomNetworkEntry {
        url: "https://app.test/x".into(),
        method: "GET".into(),
        status: 200,
        resource_type: "Fetch".into(),
        from_cache: false,
        request_id: "R-1".into(),
        ts_ms: 1,
    }];
    builder.navigate_network_entries_json = Some(serde_json::to_vec(&entries).unwrap());
    builder.navigate_network_entries_truncated = Some(true);
    let r = build_navigate_wire_receipt(&builder, "S1", Some("minimal"));
    assert!(r.network_entries.is_empty());
    assert!(r.network_entries_blob_ref.is_none());
    assert!(r.network_entries_truncated.is_none());
}

/// `capture_policy_str = Some("default")` and `Some("full")` are
/// no-ops on the wire today; Full will gain `dom_full_text`
/// semantics in a future PR.
#[test]
fn build_navigate_wire_receipt_default_and_full_are_noops() {
    let builder = navigate_builder_with_all_blobs();
    let none_r = build_navigate_wire_receipt(&builder, "S", None);
    let default_r = build_navigate_wire_receipt(&builder, "S", Some("default"));
    let full_r = build_navigate_wire_receipt(&builder, "S", Some("full"));

    let to_json = |r: &Receipt| serde_json::to_value(r).unwrap();
    assert_eq!(to_json(&none_r), to_json(&default_r));
    assert_eq!(to_json(&none_r), to_json(&full_r));
}

/// Decode-failure paths: malformed JSON in any of the three navigate
/// blobs degrades to empty/None instead of failing the navigate
/// (observability fields shouldn't trap). This pins the
/// `tracing::warn` arms.
#[test]
fn build_navigate_wire_receipt_degrades_on_malformed_console_lines_json() {
    let mut builder = navigate_builder_with_all_blobs();
    builder.navigate_console_lines_json = Some(b"not valid json".to_vec());
    let r = build_navigate_wire_receipt(&builder, "S", None);
    assert!(
        r.console_lines.is_empty(),
        "must degrade to empty, not panic"
    );
    // Other fields unaffected:
    assert_eq!(r.url.as_deref(), Some("https://example.com/"));
}

#[test]
fn build_navigate_wire_receipt_degrades_on_malformed_network_summary_json() {
    let mut builder = navigate_builder_with_all_blobs();
    builder.navigate_network_summary_json = Some(b"{not json".to_vec());
    let r = build_navigate_wire_receipt(&builder, "S", None);
    assert!(
        r.network_summary.is_none(),
        "must degrade to None, not panic"
    );
}

#[test]
fn build_navigate_wire_receipt_degrades_on_malformed_side_effects_json() {
    let mut builder = navigate_builder_with_all_blobs();
    builder.navigate_side_effects_json = Some(b"[not events".to_vec());
    let r = build_navigate_wire_receipt(&builder, "S", None);
    assert!(
        r.side_effects.is_empty(),
        "must degrade to empty, not panic"
    );
}

/// Unknown capture-policy string falls back to Default (no-op) —
/// validation is upstream in `session_validation::validate`. This
/// ensures a stale / unparseable persisted value doesn't crash
/// dispatch on an existing session.
#[test]
fn build_navigate_wire_receipt_unknown_policy_string_falls_back_to_default() {
    let builder = navigate_builder_with_all_blobs();
    let unknown = build_navigate_wire_receipt(&builder, "S", Some("bogus-profile"));
    let default = build_navigate_wire_receipt(&builder, "S", Some("default"));
    assert_eq!(
        serde_json::to_value(&unknown).unwrap(),
        serde_json::to_value(&default).unwrap(),
        "unknown policy must fall back to Default, not strip / no-op differently"
    );
}
