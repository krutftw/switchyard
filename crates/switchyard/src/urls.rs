//! URLs shown to the user: where the gateway listens and where the
//! dashboard is.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The host part of a URL someone on this machine can open for a listener
/// bound to `host`: a wildcard address becomes the loopback address of its
/// family, and an IPv6 literal is bracketed.
pub fn connect_host(host: &str) -> String {
    let trimmed = host.trim();
    let bare = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.to_string(),
        Ok(IpAddr::V4(ip)) => ip.to_string(),
        Ok(IpAddr::V6(ip)) if ip.is_unspecified() => format!("[{}]", Ipv6Addr::LOCALHOST),
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        // A host name.
        Err(_) => trimmed.to_string(),
    }
}

/// `http(s)://host:port` for a configured host and port, connectable from
/// this machine (see [`connect_host`]).
pub fn base_url(host: &str, port: u16, tls: bool) -> String {
    let scheme = if tls { "https" } else { "http" };
    format!("{scheme}://{}:{port}", connect_host(host))
}

/// [`base_url`] for a bound socket address.
pub fn base_url_of(addr: SocketAddr, tls: bool) -> String {
    base_url(&addr.ip().to_string(), addr.port(), tls)
}

/// `http(s)://ip:port` exactly as bound, wildcard included.
pub fn bound_url(addr: SocketAddr, tls: bool) -> String {
    let scheme = if tls { "https" } else { "http" };
    format!("{scheme}://{addr}")
}

/// The dashboard under a base URL.
pub fn dashboard_url(base: &str) -> String {
    format!("{}/admin/", base.trim_end_matches('/'))
}

/// Whether `host` only accepts connections from this machine. Host names
/// other than `localhost` are assumed to be reachable from elsewhere.
pub fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim();
    let bare = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_become_loopback() {
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host("::"), "[::1]");
        assert_eq!(connect_host("[::]"), "[::1]");
        assert_eq!(connect_host("192.168.1.5"), "192.168.1.5");
        assert_eq!(connect_host("::1"), "[::1]");
        assert_eq!(connect_host(" gateway.internal "), "gateway.internal");
    }

    #[test]
    fn urls() {
        assert_eq!(base_url("127.0.0.1", 8317, false), "http://127.0.0.1:8317");
        assert_eq!(base_url("0.0.0.0", 443, true), "https://127.0.0.1:443");
        let v6: SocketAddr = "[::]:9000".parse().unwrap();
        assert_eq!(base_url_of(v6, false), "http://[::1]:9000");
        assert_eq!(bound_url(v6, false), "http://[::]:9000");
        let v4: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert_eq!(base_url_of(v4, false), "http://127.0.0.1:1234");
        assert_eq!(
            dashboard_url("http://127.0.0.1:8317"),
            "http://127.0.0.1:8317/admin/"
        );
    }

    #[test]
    fn loopback_detection() {
        for host in [
            "127.0.0.1",
            "127.8.9.1",
            "::1",
            "[::1]",
            "localhost",
            " LOCALHOST ",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["0.0.0.0", "::", "192.168.0.2", "example.com", ""] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }
}
