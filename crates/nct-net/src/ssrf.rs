// SSRF guard: resolve a URL's host and refuse private/loopback/link-local
// targets unless explicitly allowed. net.fetch defaults to guarded (agents
// must not be able to probe internal networks by URL); net.http keeps raw
// curl semantics and only guards when blockPrivate is set.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use serde_json::json;

use nct_core::errors::ToolError;

/// True when the address must not be reached over the public internet.
/// Covers loopback, RFC1918, link-local, CGNAT, benchmark/test ranges,
/// IPv6 unique-local + link-local, IPv4-mapped IPv6, and the unspecified
/// address — the set an SSRF filter is expected to refuse.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()                       // 127.0.0.0/8
        || v4.is_private()                 // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local()              // 169.254/16 (cloud metadata lives here)
        || v4.is_unspecified()             // 0.0.0.0
        || o[0] == 100 && (o[1] & 0xC0) == 64   // 100.64/10 CGNAT
        || o[0] == 192 && o[1] == 0 && o[2] == 0 // 192.0.0.0/24
        || o[0] == 198 && (o[1] & 0xFE) == 18 // 198.18/15 benchmarking
}

fn is_private_v6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() {
        return true;
    }
    let seg = v6.segments();
    // IPv4-mapped (::ffff:a.b.c.d) — judge the embedded v4 address
    if seg[0..5] == [0, 0, 0, 0, 0] && seg[5] == 0xffff {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        );
        return is_private_v4(v4);
    }
    (seg[0] & 0xfe00) == 0xfc00            // fc00::/7 unique-local
        || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

/// Resolve `host:port` and return the IPs the name maps to.
pub fn resolve_host(host: &str, port: u16) -> Result<Vec<IpAddr>, ToolError> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| {
            ToolError::with_hint(
                "ERR_NET",
                format!("DNS resolution failed for {host}: {e}"),
                json!({ "host": host }),
            )
        })?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() {
        return Err(ToolError::with_hint(
            "ERR_NET",
            format!("DNS resolution returned no addresses for {host}"),
            json!({ "host": host }),
        ));
    }
    Ok(addrs)
}

/// Guard a request target: http(s) only, resolve the host, fail closed when
/// ANY resolved address is private. Returns the parsed URL on success.
pub fn assert_public(url_str: &str) -> Result<url::Url, ToolError> {
    let parsed = parse_http_url(url_str)?;
    if let Some(ip) = host_ip_literal(&parsed) {
        if is_private_ip(ip) {
            return Err(private_error(
                url_str,
                &format!("host is the private address {ip}"),
            ));
        }
        return Ok(parsed);
    }
    let host = parsed.host_str().unwrap_or_default().to_string();
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    let ips = resolve_host(&host, port)?;
    if let Some(bad) = ips.iter().find(|ip| is_private_ip(**ip)) {
        return Err(private_error(
            url_str,
            &format!("host {host} resolves to the private address {bad}"),
        ));
    }
    Ok(parsed)
}

/// Guard a redirect hop: same checks as assert_public but the error names the
/// hop, so audit logs show exactly where a redirect tried to escape to.
pub fn assert_public_redirect(url_str: &str, hop: usize) -> Result<url::Url, ToolError> {
    assert_public(url_str).map_err(|e| {
        let msg = format!("redirect hop {hop}: {}", e.message);
        ToolError::with_hint(&e.code, msg, json!({ "url": url_str, "hop": hop }))
    })
}

pub fn parse_http_url(url_str: &str) -> Result<url::Url, ToolError> {
    let parsed = url::Url::parse(url_str).map_err(|e| {
        ToolError::with_hint(
            "ERR_BAD_INPUT",
            format!("invalid url: {e}"),
            json!({ "got": url_str }),
        )
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ToolError::with_hint(
            "ERR_BAD_INPUT",
            "url must be an http(s) URL",
            json!({ "got": url_str }),
        ));
    }
    if parsed.host_str().map(|h| h.is_empty()).unwrap_or(true) {
        return Err(ToolError::with_hint(
            "ERR_BAD_INPUT",
            "url has no host",
            json!({ "got": url_str }),
        ));
    }
    Ok(parsed)
}

fn host_ip_literal(u: &url::Url) -> Option<IpAddr> {
    match u.host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    }
}

fn private_error(url_str: &str, reason: &str) -> ToolError {
    ToolError::with_hint(
        "ERR_SSRF_BLOCKED",
        format!(
            "refusing to fetch {url_str}: {reason} (pass allowPrivate to reach internal hosts)"
        ),
        json!({ "url": url_str, "reason": reason }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_fail_closed() {
        for s in [
            "http://127.0.0.1:8080/",
            "http://10.0.0.1/",
            "http://172.16.0.9/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://100.64.1.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            let err = assert_public(s).unwrap_err();
            assert_eq!(err.code, "ERR_SSRF_BLOCKED", "{s} must be blocked");
        }
    }

    #[test]
    fn public_literal_passes_without_dns() {
        assert!(assert_public("http://93.184.216.34/").is_ok());
        assert!(assert_public("http://[2606:2800:220:1:248:1893:25c8:1946]/").is_ok());
    }

    #[test]
    fn non_http_schemes_rejected() {
        assert_eq!(
            assert_public("file:///etc/passwd").unwrap_err().code,
            "ERR_BAD_INPUT"
        );
        assert_eq!(
            assert_public("ftp://example.com/").unwrap_err().code,
            "ERR_BAD_INPUT"
        );
    }
}
