// The `net-request` host function's SSRF guard and HTTP client: outbound URL
// validation, DNS resolution with blocked-range checks, bounded redirects.
// Split out of `host_impl.rs`.

use loom_core::vault::NetRequest;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Maximum redirect hops followed manually (matches reqwest's historical
/// default of 10).
const MAX_REDIRECT_HOPS: usize = 10;

/// Returns `true` for IPs a guest must never be able to reach: loopback,
/// private/RFC1918, link-local (incl. the 169.254.169.254 cloud-metadata
/// endpoint), unspecified, ULA (fc00::/7), IPv4-mapped/compat IPv6 of any of
/// the above, plus broadcast / documentation / benchmarking ranges. Checked
/// AFTER DNS resolution so a public hostname that resolves to an internal IP
/// (DNS rebind) is still rejected.
fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_blocked(v4),
        IpAddr::V6(v6) => {
            // Unwrap IPv4-MAPPED addresses (::ffff:a.b.c.d) so an attacker
            // can't smuggle 127.0.0.1 in as ::ffff:127.0.0.1. Note: we use
            // `to_ipv4_mapped` (not `to_ipv4`) on purpose — `to_ipv4` also
            // maps the deprecated IPv4-compatible form, which would turn ::1
            // into 0.0.0.1 and slip loopback past the v4 check.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ipv4_is_blocked(mapped);
            }
            ipv6_is_blocked(v6)
        }
    }
}

fn ipv4_is_blocked(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()            // 127.0.0.0/8
        || v4.is_private()      // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local()  // 169.254/16 (incl. 169.254.169.254 metadata)
        || v4.is_unspecified() // 0.0.0.0
        || v4.is_broadcast()   // 255.255.255.255
        || v4.is_documentation()
        // 100.64.0.0/10 — carrier-grade NAT / shared address space.
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        // 192.0.0.0/24 — IETF protocol assignments.
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 198.18.0.0/15 — benchmarking.
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        // 240.0.0.0/4 — reserved (excludes the broadcast addr handled above).
        || o[0] >= 240
}

fn ipv6_is_blocked(v6: Ipv6Addr) -> bool {
    v6.is_loopback()            // ::1
        || v6.is_unspecified() // ::
        // fc00::/7 — unique local addresses.
        || (v6.segments()[0] & 0xfe00) == 0xfc00
        // fe80::/10 — link-local.
        || (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// Pure SSRF policy: validate a URL plus the socket addrs it resolved to.
/// `resolved` must be the full set of addrs the connection may use (the same
/// set later pinned via `resolve`). Returns `Ok(())` only if the scheme is
/// allowed AND every resolved IP is outside the blocked ranges.
///
/// Pure and side-effect-free so it can be unit-tested directly (the host
/// path additionally resolves DNS and pins the connection to `resolved`).
fn validate_outbound_url(url: &reqwest::Url, resolved: &[SocketAddr]) -> Result<(), String> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "{{\"kind\":\"url_blocked\",\"reason\":\"scheme_not_allowed\",\"scheme\":\"{scheme}\"}}"
        ));
    }
    if resolved.is_empty() {
        return Err("{\"kind\":\"url_blocked\",\"reason\":\"dns_no_addresses\"}".to_owned());
    }
    for addr in resolved {
        if ip_is_blocked(addr.ip()) {
            return Err(format!(
                "{{\"kind\":\"url_blocked\",\"reason\":\"private_or_loopback_ip\",\"ip\":\"{}\"}}",
                addr.ip()
            ));
        }
    }
    Ok(())
}

/// Resolve a URL's host to socket addrs and run the SSRF policy. Returns the
/// validated addrs (to be pinned via `resolve`) on success.
async fn resolve_and_validate(url: &reqwest::Url) -> Result<Vec<SocketAddr>, String> {
    // A literal-IP host is parsed directly (no DNS); otherwise look it up.
    let host = url
        .host_str()
        .ok_or_else(|| "{\"kind\":\"url_blocked\",\"reason\":\"missing_host\"}".to_owned())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "{\"kind\":\"url_blocked\",\"reason\":\"unknown_port\"}".to_owned())?;

    let resolved: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| {
                format!(
                    "{{\"kind\":\"url_blocked\",\"reason\":\"dns_resolution_failed\",\"detail\":\"{e}\"}}"
                )
            })?
            .collect()
    };

    validate_outbound_url(url, &resolved)?;
    Ok(resolved)
}

/// Perform a real HTTP request using reqwest (pure-Rust, rustls-tls).
/// Returns a loom-core NetResp on success or an error string on failure.
///
/// SSRF-guarded: resolves+validates the URL (and every redirect hop) and pins
/// the connection to the validated IPs so a guest cannot reach internal
/// targets. See the SSRF-guard comment block above.
pub(crate) async fn do_http_request(req: NetRequest) -> Result<loom_core::vault::NetResp, String> {
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| e.to_string())?;
    let mut current_url = reqwest::Url::parse(&req.url).map_err(|e| e.to_string())?;

    for _hop in 0..=MAX_REDIRECT_HOPS {
        // Resolve + validate THIS hop, then pin the connection to the
        // validated addrs so reqwest cannot re-resolve to a different
        // (internal) IP between our check and the connect.
        let resolved = resolve_and_validate(&current_url).await?;
        let host = current_url
            .host_str()
            .ok_or_else(|| "{\"kind\":\"url_blocked\",\"reason\":\"missing_host\"}".to_owned())?
            .to_owned();

        let mut builder = reqwest::Client::builder()
            .use_rustls_tls()
            // Disable auto-redirect: we follow Location manually so the guard
            // re-runs on every hop.
            .redirect(reqwest::redirect::Policy::none());
        for addr in &resolved {
            builder = builder.resolve(&host, *addr);
        }
        let client = builder.build().map_err(|e| e.to_string())?;

        let mut rb = client.request(method.clone(), current_url.clone());
        for (k, v) in &req.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        // Vault-substituted bearer token: applied AFTER the plain headers so it
        // wins over any guest-supplied Authorization value (the historical
        // overwrite-in-map semantics). `expose()` here is the single wire-out
        // site; the value never reaches Debug/Serialize (G1/TB4).
        if let Some(auth) = &req.authorization {
            rb = rb.header("Authorization", auth.expose().as_str());
        }
        if !req.body.is_empty() {
            rb = rb.body(req.body.clone());
        }

        let response = rb.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();

        // Manual redirect handling: on a 3xx with a Location, validate the
        // next hop and loop; otherwise return the response.
        if response.status().is_redirection() {
            if let Some(loc) = response.headers().get(reqwest::header::LOCATION) {
                let loc = loc.to_str().map_err(|e| {
                    format!("{{\"kind\":\"url_blocked\",\"reason\":\"bad_location_header\",\"detail\":\"{e}\"}}")
                })?;
                current_url = current_url.join(loc).map_err(|e| {
                    format!("{{\"kind\":\"url_blocked\",\"reason\":\"bad_redirect_target\",\"detail\":\"{e}\"}}")
                })?;
                continue;
            }
            // 3xx without Location: nothing to follow, return as-is.
        }

        let headers: std::collections::BTreeMap<String, String> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();

        return Ok(loom_core::vault::NetResp {
            status,
            headers,
            body,
        });
    }

    Err("{\"kind\":\"url_blocked\",\"reason\":\"too_many_redirects\"}".to_owned())
}

#[cfg(test)]
mod ssrf_guard_tests {
    use super::{ip_is_blocked, validate_outbound_url};
    use std::net::{IpAddr, SocketAddr};

    fn addr(s: &str) -> SocketAddr {
        SocketAddr::new(s.parse::<IpAddr>().unwrap(), 80)
    }

    #[test]
    fn blocks_loopback_private_linklocal_and_metadata() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.5.4",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata endpoint
            "169.254.0.1",
            "0.0.0.0",
            "100.64.0.1", // CGN/shared
            "198.18.0.1", // benchmarking
            "240.0.0.1",  // reserved
            "::1",
            "fc00::1",          // ULA
            "fd00::1",          // ULA
            "fe80::1",          // link-local
            "::ffff:127.0.0.1", // IPv4-mapped loopback (smuggling)
            "::ffff:169.254.169.254",
        ] {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip} must be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(!ip_is_blocked(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    #[test]
    fn validate_rejects_non_http_scheme() {
        let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
        let err = validate_outbound_url(&url, &[addr("8.8.8.8")]).unwrap_err();
        assert!(err.contains("scheme_not_allowed"), "got: {err}");
    }

    #[test]
    fn validate_rejects_resolved_internal_ip_dns_rebind() {
        // A perfectly public-looking https URL whose host resolved (via DNS,
        // simulated here) to an internal IP must be rejected — this is the
        // DNS-rebind defense: the verdict is on the RESOLVED addr, not the
        // hostname.
        let url = reqwest::Url::parse("https://totally-public.example/").unwrap();
        let err = validate_outbound_url(&url, &[addr("169.254.169.254")]).unwrap_err();
        assert!(err.contains("private_or_loopback_ip"), "got: {err}");
        assert!(err.contains("169.254.169.254"), "names the IP: {err}");
    }

    #[test]
    fn validate_rejects_when_any_resolved_addr_is_blocked() {
        // Multi-A-record rebind: one public + one internal addr must still be
        // rejected (we'd pin all of them, so any internal one is reachable).
        let url = reqwest::Url::parse("https://mixed.example/").unwrap();
        let err = validate_outbound_url(&url, &[addr("8.8.8.8"), addr("127.0.0.1")]).unwrap_err();
        assert!(err.contains("private_or_loopback_ip"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_resolution() {
        let url = reqwest::Url::parse("https://nxdomain.example/").unwrap();
        let err = validate_outbound_url(&url, &[]).unwrap_err();
        assert!(err.contains("dns_no_addresses"), "got: {err}");
    }

    #[test]
    fn validate_passes_public_https() {
        let url = reqwest::Url::parse("https://example.com/path").unwrap();
        assert!(validate_outbound_url(&url, &[addr("93.184.216.34")]).is_ok());
    }
}
