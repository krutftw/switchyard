//! Outbound proxy selection.
//!
//! A proxy can be configured per credential, per provider and globally
//! (`upstream.proxy`); the most specific non-empty setting wins. The value
//! grammar is [`switchyard_core::config::parse_proxy`]: empty = inherit,
//! `direct` = no proxy at all (environment variables ignored too), or an
//! `http`, `https`, `socks5`, `socks5h` URL.

use std::net::IpAddr;
use switchyard_core::config::{ProxySetting, parse_proxy};

/// Resolves the effective proxy for one upstream call.
///
/// Precedence: credential, then provider, then the global setting. A level
/// that is empty — or invalid, which config validation normally prevents —
/// falls through to the next one. When every level inherits the result is
/// [`ProxySetting::Inherit`], meaning "use the process environment"
/// (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`).
pub fn resolve_proxy(credential: &str, provider: &str, global: &str) -> ProxySetting {
    for (level, value) in [
        ("credential", credential),
        ("provider", provider),
        ("upstream", global),
    ] {
        match parse_proxy(value) {
            Ok(ProxySetting::Inherit) => {}
            Ok(setting) => return setting,
            Err(reason) => {
                tracing::warn!(
                    level,
                    proxy = %redact_proxy_url(value),
                    "ignoring invalid proxy setting: {reason}"
                );
            }
        }
    }
    ProxySetting::Inherit
}

/// Renders a proxy URL for logs: `scheme://[redacted@]host[:port]`, dropping
/// credentials, path and query.
pub fn redact_proxy_url(value: &str) -> String {
    let value = value.trim();
    match url::Url::parse(value) {
        Ok(u) => {
            let mut out = format!("{}://", u.scheme());
            if !u.username().is_empty() || u.password().is_some() {
                out.push_str("redacted@");
            }
            out.push_str(u.host_str().unwrap_or(""));
            if let Some(port) = u.port() {
                out.push(':');
                out.push_str(&port.to_string());
            }
            out
        }
        Err(_) => "<invalid proxy url>".to_string(),
    }
}

/// A stable cache key for a proxy setting.
pub(crate) fn proxy_cache_key(setting: &ProxySetting) -> String {
    match setting {
        ProxySetting::Inherit => "inherit".to_string(),
        ProxySetting::Direct => "direct".to_string(),
        ProxySetting::Url(u) => format!("url:{}", u.trim()),
    }
}

/// Picks a proxy from environment-style variables for a destination, the way
/// curl does: `HTTPS_PROXY` for TLS destinations, `HTTP_PROXY` for plain
/// ones, `ALL_PROXY` as the fallback, lower-case spellings accepted, and
/// `NO_PROXY` as an exclusion list (`*`, exact hosts, domain suffixes with or
/// without a leading dot, optional `:port`, and address blocks such as
/// `10.0.0.0/8`).
///
/// reqwest applies the same rules itself for HTTP calls; this exists for
/// WebSocket connections, which are dialled by hand. `lookup` abstracts the
/// environment so the rules can be tested.
pub(crate) fn proxy_from_env_with(
    lookup: impl Fn(&str) -> Option<String>,
    tls: bool,
    host: &str,
    port: u16,
) -> Option<String> {
    let get = |names: &[&str]| {
        names
            .iter()
            .filter_map(|n| lookup(n))
            .map(|v| v.trim().to_string())
            .find(|v| !v.is_empty())
    };
    if get(&["NO_PROXY", "no_proxy"]).is_some_and(|list| no_proxy_matches(&list, host, port)) {
        return None;
    }
    let specific: &[&str] = if tls {
        &["HTTPS_PROXY", "https_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy"]
    };
    let raw = get(specific).or_else(|| get(&["ALL_PROXY", "all_proxy"]))?;
    // Environment values are often written without a scheme (`proxy:3128`).
    let candidate = if raw.contains("://") {
        raw
    } else {
        format!("http://{raw}")
    };
    match parse_proxy(&candidate) {
        Ok(ProxySetting::Url(u)) => Some(u),
        _ => None,
    }
}

/// [`proxy_from_env_with`] over the real process environment.
pub(crate) fn proxy_from_env(tls: bool, host: &str, port: u16) -> Option<String> {
    proxy_from_env_with(|name| std::env::var(name).ok(), tls, host, port)
}

fn no_proxy_matches(list: &str, host: &str, port: u16) -> bool {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    for entry in list.split(',') {
        let entry = entry.trim().to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            return true;
        }
        // `host:port` entries only apply to that port. A bare IPv6 literal
        // also contains colons, so only split when the tail is numeric and
        // the head has no other colon.
        let (name, entry_port) = match entry.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), p.parse::<u16>().ok())
            }
            _ => (entry.clone(), None),
        };
        if entry_port.is_some_and(|p| p != port) {
            continue;
        }
        let name = name
            .trim_start_matches("*.")
            .trim_start_matches('.')
            .trim_start_matches('[')
            .trim_end_matches(']');
        if name.is_empty() {
            continue;
        }
        // `10.0.0.0/8`, `fd00::/8`: an address block, as the HTTP client
        // understands it too.
        if let Some(inside) = cidr_contains(name, &host) {
            if inside {
                return true;
            }
            continue;
        }
        if host == name || host.ends_with(&format!(".{name}")) {
            return true;
        }
    }
    false
}

/// Whether the IP literal `host` lies in the CIDR block `entry`. `None` when
/// `entry` is not a CIDR block at all.
fn cidr_contains(entry: &str, host: &str) -> Option<bool> {
    let (network, bits) = entry.split_once('/')?;
    let network: IpAddr = network.parse().ok()?;
    let bits: u32 = bits.parse().ok()?;
    let Ok(host) = host.parse::<IpAddr>() else {
        // A host name is never inside an address block.
        return Some(false);
    };
    Some(match (network, host) {
        (IpAddr::V4(network), IpAddr::V4(host)) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(network) & mask == u32::from(host) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(host)) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(network) & mask == u128::from(host) & mask
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn precedence_is_credential_then_provider_then_global() {
        assert_eq!(
            resolve_proxy("socks5://c:1080", "http://p:3128", "http://g:3128"),
            ProxySetting::Url("socks5://c:1080".into())
        );
        assert_eq!(
            resolve_proxy("", "http://p:3128", "http://g:3128"),
            ProxySetting::Url("http://p:3128".into())
        );
        assert_eq!(
            resolve_proxy("", "", "http://g:3128"),
            ProxySetting::Url("http://g:3128".into())
        );
        assert_eq!(resolve_proxy("", "", ""), ProxySetting::Inherit);
    }

    #[test]
    fn direct_at_a_specific_level_beats_a_global_proxy() {
        assert_eq!(
            resolve_proxy("direct", "", "http://g:3128"),
            ProxySetting::Direct
        );
        assert_eq!(
            resolve_proxy("", "NONE", "http://g:3128"),
            ProxySetting::Direct
        );
        assert_eq!(resolve_proxy(" ", " ", "direct"), ProxySetting::Direct);
    }

    #[test]
    fn invalid_levels_fall_through() {
        assert_eq!(
            resolve_proxy("ftp://nope", "", "http://g:3128"),
            ProxySetting::Url("http://g:3128".into())
        );
        assert_eq!(resolve_proxy("not a url", "", ""), ProxySetting::Inherit);
    }

    #[test]
    fn redaction_drops_credentials_and_paths() {
        assert_eq!(
            redact_proxy_url("http://user:secret@proxy.local:3128/path?x=1"),
            "http://redacted@proxy.local:3128"
        );
        assert_eq!(
            redact_proxy_url("socks5h://10.0.0.1:1080"),
            "socks5h://10.0.0.1:1080"
        );
        assert_eq!(redact_proxy_url("::::"), "<invalid proxy url>");
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn env_proxy_selection_by_scheme() {
        let e = env(&[
            ("HTTPS_PROXY", "http://secure:3128"),
            ("http_proxy", "http://plain:3128"),
        ]);
        assert_eq!(
            proxy_from_env_with(&e, true, "api.openai.com", 443).as_deref(),
            Some("http://secure:3128")
        );
        assert_eq!(
            proxy_from_env_with(&e, false, "example.com", 80).as_deref(),
            Some("http://plain:3128")
        );
    }

    #[test]
    fn env_all_proxy_is_the_fallback_and_schemeless_values_are_http() {
        let e = env(&[("ALL_PROXY", "socks5h://tor:9050")]);
        assert_eq!(
            proxy_from_env_with(&e, true, "x.test", 443).as_deref(),
            Some("socks5h://tor:9050")
        );
        let e = env(&[("https_proxy", "corp-proxy:8080")]);
        assert_eq!(
            proxy_from_env_with(&e, true, "x.test", 443).as_deref(),
            Some("http://corp-proxy:8080")
        );
        let e = env(&[]);
        assert_eq!(proxy_from_env_with(&e, true, "x.test", 443), None);
    }

    #[test]
    fn env_no_proxy_rules() {
        let base = [
            ("HTTPS_PROXY", "http://p:1"),
            (
                "NO_PROXY",
                "localhost, .internal.example ,*.corp.test,10.0.0.5,api.special.test:8443",
            ),
        ];
        let e = env(&base);
        let hit = |host: &str, port: u16| proxy_from_env_with(&e, true, host, port).is_some();
        assert!(!hit("localhost", 443));
        assert!(!hit("svc.internal.example", 443));
        assert!(!hit("internal.example", 443));
        assert!(!hit("a.b.corp.test", 443));
        assert!(!hit("10.0.0.5", 443));
        assert!(!hit("api.special.test", 8443));
        assert!(hit("api.special.test", 443));
        assert!(hit("notinternal.example", 443));
        assert!(hit("api.openai.com", 443));

        let e = env(&[("HTTPS_PROXY", "http://p:1"), ("no_proxy", "*")]);
        assert_eq!(proxy_from_env_with(&e, true, "anything.test", 443), None);
    }

    #[test]
    fn env_no_proxy_understands_address_blocks() {
        let e = env(&[
            ("HTTPS_PROXY", "http://p:1"),
            (
                "NO_PROXY",
                "10.0.0.0/8, 192.168.1.0/24,fd00::/8,203.0.113.7/32",
            ),
        ]);
        let proxied = |host: &str| proxy_from_env_with(&e, true, host, 443).is_some();
        assert!(!proxied("10.1.2.3"));
        assert!(!proxied("10.255.255.255"));
        assert!(!proxied("192.168.1.77"));
        assert!(!proxied("fd12:3456::1"));
        assert!(!proxied("[fd12:3456::1]"));
        assert!(!proxied("203.0.113.7"));
        assert!(proxied("11.0.0.1"));
        assert!(proxied("192.168.2.1"));
        assert!(proxied("203.0.113.8"));
        assert!(proxied("2001:db8::1"));
        // A host name is never inside an address block.
        assert!(proxied("10.example.com"));

        assert_eq!(cidr_contains("0.0.0.0/0", "8.8.8.8"), Some(true));
        assert_eq!(cidr_contains("::/0", "2001:db8::1"), Some(true));
        assert_eq!(cidr_contains("10.0.0.0/8", "::1"), Some(false));
        assert_eq!(cidr_contains("10.0.0.0/33", "10.0.0.1"), Some(false));
        assert_eq!(cidr_contains("example.com/8", "10.0.0.1"), None);
        assert_eq!(cidr_contains("10.0.0.1", "10.0.0.1"), None);
    }
}
