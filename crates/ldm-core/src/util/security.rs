//! URL policy for untrusted (extension / web page originated) requests
//!.
use std::net::IpAddr;

use crate::error::{LdmError, Result};

/// Validate a download URL: only http/https/ftp(s/es), must have a host, and
/// optionally reject link-local / metadata endpoints.
pub fn validate_url(u: &str, block_link_local: bool) -> Result<url::Url> {
    let parsed = url::Url::parse(u).map_err(|e| LdmError::Security(format!("invalid URL: {e}")))?;
    match parsed.scheme() {
        "http" | "https" | "ftp" | "ftps" | "ftpes" => {}
        s => return Err(LdmError::Security(format!("scheme '{s}' is not allowed"))),
    }
    let host = parsed.host().ok_or_else(|| LdmError::Security("URL has no host".into()))?;
    if block_link_local && host_blocked(&host) {
        return Err(LdmError::Security(format!("blocked address {host}")));
    }
    Ok(parsed)
}

pub fn host_blocked(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => ip_blocked(&IpAddr::V4(*ip)),
        url::Host::Ipv6(ip) => ip_blocked(&IpAddr::V6(*ip)),
        url::Host::Domain(d) => {
            let d = d.to_ascii_lowercase();
            d == "metadata.google.internal" || d == "metadata"
        }
    }
}

pub fn ip_blocked(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local() || v4.is_unspecified() || v4.octets() == [100, 100, 100, 200],
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80 || v6.is_unspecified(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy() {
        assert!(validate_url("http://127.0.0.1:8000/a", true).is_ok());
        assert!(validate_url("https://example.com/a", true).is_ok());
        assert!(validate_url("file:///etc/passwd", true).is_err());
        assert!(validate_url("chrome://settings", true).is_err());
        assert!(validate_url("ftp://h/a", true).is_ok());
        assert!(validate_url("file:///etc/passwd", true).is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data", true).is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data", false).is_ok());
        assert!(validate_url("http://[fe80::1]/", true).is_err());
        assert!(validate_url("http://metadata.google.internal/", true).is_err());
        assert!(validate_url("not a url", true).is_err());
    }
}

/// Log redaction: query values (and userinfo) of every http(s)/ftp URL in `text`
/// become `…`, so signed links / tokens do not end up in log files. Parameter
/// names stay for debugging.
pub fn redact_url(text: &str) -> String {
    const SCHEMES: [&str; 5] = ["http://", "https://", "ftp://", "ftps://", "ftpes://"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = SCHEMES.iter().filter_map(|s| rest.find(s)).min() {
        let (head, tail) = rest.split_at(i);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ',')).unwrap_or(tail.len());
        let (url, after) = tail.split_at(end);
        // credentials in the authority (ftp://user:pass@host) are hidden as well
        let scheme_end = url.find("://").map(|p| p + 3).unwrap_or(0);
        let auth_end = url[scheme_end..].find(['/', '?', '#']).map(|p| p + scheme_end).unwrap_or(url.len());
        let url: String = match url[scheme_end..auth_end].rfind('@') {
            Some(at) => format!("{}…@{}", &url[..scheme_end], &url[scheme_end + at + 1..]),
            None => url.to_string(),
        };
        match url.split_once('?') {
            Some((base, q)) => {
                let (q, frag) = q.split_once('#').map(|(a, b)| (a, Some(b))).unwrap_or((q, None));
                out.push_str(base);
                out.push('?');
                let parts: Vec<String> = q
                    .split('&')
                    .map(|kv| match kv.split_once('=') {
                        Some((k, _)) => format!("{k}=…"),
                        None => kv.to_string(),
                    })
                    .collect();
                out.push_str(&parts.join("&"));
                if frag.is_some() {
                    out.push_str("#…");
                }
            }
            None => out.push_str(&url),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod redact_tests {
    use super::redact_url;

    #[test]
    fn redacts_query_values_only() {
        assert_eq!(redact_url("GET https://h/a.mp4?token=abc&x=1#t done"), "GET https://h/a.mp4?token=…&x=…#… done");
        assert_eq!(redact_url("plain http://h/p no query"), "plain http://h/p no query");
        assert_eq!(redact_url("httpd https://a/?s"), "httpd https://a/?s");
        assert_eq!(redact_url("get ftp://bob:pw@h:21/f.iso ok"), "get ftp://…@h:21/f.iso ok");
        assert_eq!(redact_url("https://h/p@x"), "https://h/p@x", "an @ in the path is not userinfo");
        assert_eq!(redact_url("(http://h/x?sig=1), next"), "(http://h/x?sig=…), next");
    }
}
