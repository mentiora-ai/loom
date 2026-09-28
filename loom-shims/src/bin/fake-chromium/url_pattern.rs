//! The URL conventions `Page.navigate` pattern-matches in place of a network.

/// URL-driven test scaffolding for navigate-error and blocklist behaviours.
/// Real Chromium would fetch the URL from the network; fake-chromium
/// pattern-matches the URL string and emits the corresponding CDP events
/// synthetically.
pub(crate) enum FakeUrlPattern {
    /// `http://fake.test/status/<N>` → emit Network.responseReceived with
    /// the parsed HTTP status.
    Status(u16),
    /// `http://fake.test/error/<CDP>` → emit Network.loadingFailed AND
    /// set errorText=<CDP> in the Page.navigate response.
    Error(String),
    /// `http://fake.test/page-with-tracker` → after
    /// Page.navigate, emit `Fetch.requestPaused` events for the
    /// document AND for a hard-coded analytics sub-resource
    /// (`https://www.google-analytics.com/analytics.js`). The document
    /// event has `resourceType="Document"` and is first per-frame so
    /// the interceptor's frameId-based skip-gate lets it through; the
    /// sub-resource event has `resourceType="Script"` and matches the
    /// default blocklist → must be answered with `Fetch.failRequest`.
    PageWithTracker,
    /// `http://fake.test/page-with-iframe-404` → emit the MAIN
    /// document's 200 responseReceived (frameId/loaderId matching the
    /// canned `Page.navigate` response: fake-frame-1/fake-loader-1)
    /// PLUS an IFRAME document's 404 responseReceived under a
    /// different frameId/loaderId. The navigate must SUCCEED with
    /// status_code=200 — the iframe 404 stays in `network_events` for
    /// observability only (main-document failure scoping).
    PageWithIframe404,
    /// `http://fake.test/slow/<MS>` → sleep <MS> milliseconds BEFORE sending
    /// the `Page.navigate` response, so a shim per-CDP-command navigate-budget
    /// timeout (`LOOM_SHIM_CDP_TIMEOUT_MS`) fires deterministically. The delay
    /// is on the navigate command's own response — the binding CDP roundtrip.
    Slow(u64),
    /// Anything else — emit no synthetic Network event (status_code
    /// will remain 0 from the shim's perspective, mirroring real
    /// Chromium with caching disabled).
    None,
}

pub(crate) fn parse_fake_url_pattern(url: &str) -> FakeUrlPattern {
    if let Some(rest) = url
        .strip_prefix("http://fake.test/status/")
        .or_else(|| url.strip_prefix("https://fake.test/status/"))
    {
        let n: &str = rest.split('?').next().unwrap_or("");
        if let Ok(status) = n.parse::<u16>() {
            return FakeUrlPattern::Status(status);
        }
    }
    if let Some(rest) = url
        .strip_prefix("http://fake.test/error/")
        .or_else(|| url.strip_prefix("https://fake.test/error/"))
    {
        let code = rest.split('?').next().unwrap_or("");
        if !code.is_empty() {
            return FakeUrlPattern::Error(code.to_string());
        }
    }
    if let Some(rest) = url
        .strip_prefix("http://fake.test/slow/")
        .or_else(|| url.strip_prefix("https://fake.test/slow/"))
    {
        let n: &str = rest.split('?').next().unwrap_or("");
        if let Ok(ms) = n.parse::<u64>() {
            return FakeUrlPattern::Slow(ms);
        }
    }
    if url == "http://fake.test/page-with-tracker" || url == "https://fake.test/page-with-tracker" {
        return FakeUrlPattern::PageWithTracker;
    }
    // BLOCKLISTED-host variant: same tracker page served from a host that
    // matches the default blocklist (`*.google-analytics.com`). The
    // document's own Fetch.requestPaused URL then matches the blocklist,
    // exercising the interceptor's main-frame skip-gate — the documented
    // 'operator's primary URL is never gated' invariant — on EVERY
    // navigate of a session, not just the first.
    if url == "https://www.google-analytics.com/page-with-tracker" {
        return FakeUrlPattern::PageWithTracker;
    }
    if url == "http://fake.test/page-with-iframe-404"
        || url == "https://fake.test/page-with-iframe-404"
    {
        return FakeUrlPattern::PageWithIframe404;
    }
    FakeUrlPattern::None
}
