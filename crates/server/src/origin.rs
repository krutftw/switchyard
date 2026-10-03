//! Telling requests made by web pages of other origins from all others.
//!
//! `server.cors = false` is how an operator says "no use of this gateway by
//! other sites' pages". Leaving out the CORS headers only keeps such a page
//! from *reading* an answer, and only over plain HTTP: a browser sends a
//! WebSocket handshake to any host without asking and lets the page read
//! everything that comes back, and it sends "simple" POST requests
//! unasked, too, which run — and cost — whether or not the page gets to see
//! the result. Where the network position is the credential
//! (`auth.required = false` on a loopback listener), every page opened in a
//! browser on that machine could use the gateway that way.
//!
//! So with CORS off, requests that do something (everything but the model
//! listings) are refused when a browser says they come from a page of
//! another origin. Clients that are not browsers send neither of the
//! headers looked at here and are never affected.

use http::header::{HOST, ORIGIN};
use http::request::Parts;
use http::uri::Authority;
use http::{HeaderValue, Uri};
use std::net::IpAddr;

/// Anonymous access uses the listener's network position as its credential.
/// Do not trust a browser's same-origin verdict for an arbitrary DNS name.
/// Authenticated reverse-proxy clients do not use this restriction.
pub(crate) fn anonymous_host_is_allowed(parts: &Parts, configured_host: &str) -> bool {
    let allowed = |host: &str| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let configured = configured_host
            .trim_start_matches('[')
            .trim_end_matches(']');
        host.eq_ignore_ascii_case(configured)
            || host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|address| {
                address.to_canonical().is_loopback()
                    || configured
                        .parse::<IpAddr>()
                        .is_ok_and(|bound| bound.is_unspecified())
            })
    };
    let mut hosts = parts.headers.get_all(HOST).iter();
    if let Some(host) = hosts.next()
        && (hosts.next().is_some()
            || !host
                .to_str()
                .ok()
                .and_then(|host| host.parse::<Authority>().ok())
                .is_some_and(|host| allowed(host.host())))
    {
        return false;
    }
    if parts
        .uri
        .authority()
        .is_some_and(|host| !allowed(host.host()))
    {
        return false;
    }
    // Also check the browser origin when a local reverse proxy rewrites Host.
    parts.headers.get_all(ORIGIN).iter().all(|origin| {
        origin
            .to_str()
            .ok()
            .and_then(|origin| origin.parse::<Uri>().ok())
            .and_then(|origin| origin.host().map(str::to_owned))
            .is_some_and(|host| allowed(&host))
    })
}

/// Whether the request was made by a web page whose origin is not this
/// server's, as far as the browser tells.
///
/// * `Sec-Fetch-Site`, which browsers set and pages cannot, is the
///   browser's own verdict: `same-origin` (and `none`, a request the user
///   made, not a page) is fine, `cross-site` and `same-site` are not.
/// * Without it (older browsers, WebSocket handshakes of some), an `Origin`
///   that does not name the host the request was sent to is another origin.
/// * Neither header: not a browser, or nothing a page could have caused.
pub(crate) fn is_foreign_page(parts: &Parts) -> bool {
    let verdict = parts
        .headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase());
    if let Some(verdict) = verdict {
        return !matches!(verdict.as_str(), "same-origin" | "none");
    }

    let mut origins = parts.headers.get_all(ORIGIN).iter().peekable();
    if origins.peek().is_none() {
        return false;
    }
    // HTTP/2 carries the host in the request target instead of a header.
    let host = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<Authority>().ok())
        .or_else(|| parts.uri.authority().cloned());
    let Some(host) = host else {
        return true;
    };
    !origins.all(|origin| names_host(origin, &host))
}

/// The port a scheme implies.
fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

/// Whether an `Origin` value names `host`: the same host name and the same
/// port, where a port left out is the default of the origin's scheme.
///
/// The scheme itself is not compared: behind a TLS-terminating proxy the
/// page's origin is `https://…` while this server speaks plain HTTP. The
/// opaque origin `null` (sandboxed frames, local files) names nothing.
fn names_host(origin: &HeaderValue, host: &Authority) -> bool {
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.trim().parse::<Uri>().ok())
    else {
        return false;
    };
    let (Some(scheme), Some(origin_host)) = (origin.scheme_str(), origin.host()) else {
        return false;
    };
    let implied = default_port(&scheme.to_ascii_lowercase());
    origin_host.eq_ignore_ascii_case(host.host())
        && origin.port_u16().or(implied) == host.port_u16().or(implied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = http::Request::builder().uri("/v1/responses");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn foreign(headers: &[(&str, &str)]) -> bool {
        is_foreign_page(&parts(headers))
    }

    #[test]
    fn clients_that_are_not_browsers_are_never_foreign_pages() {
        assert!(!foreign(&[]));
        assert!(!foreign(&[("host", "gateway.example:8317")]));
        assert!(!foreign(&[("authorization", "Bearer sy-key")]));
    }

    #[test]
    fn anonymous_names_are_bound_to_the_listener() {
        for host in ["127.0.0.1:8317", "[::1]:8317", "localhost:8317"] {
            assert!(anonymous_host_is_allowed(
                &parts(&[("host", host)]),
                "127.0.0.1"
            ));
        }
        assert!(!anonymous_host_is_allowed(
            &parts(&[
                ("host", "unconfigured.example:8317"),
                ("sec-fetch-site", "same-origin")
            ]),
            "127.0.0.1"
        ));
        assert!(!anonymous_host_is_allowed(
            &parts(&[
                ("host", "localhost:8317"),
                ("origin", "https://unconfigured.example"),
                ("sec-fetch-site", "same-origin")
            ]),
            "127.0.0.1"
        ));
        assert!(anonymous_host_is_allowed(
            &parts(&[("host", "gateway.example:8317")]),
            "gateway.example"
        ));
        assert!(!anonymous_host_is_allowed(
            &parts(&[("host", "192.0.2.1:8317")]),
            "127.0.0.1"
        ));
        assert!(anonymous_host_is_allowed(
            &parts(&[("host", "192.0.2.1:8317")]),
            "0.0.0.0"
        ));
    }

    #[test]
    fn an_origin_is_compared_with_the_host_the_request_was_sent_to() {
        let same = [
            ("127.0.0.1:8317", "http://127.0.0.1:8317"),
            ("localhost:8317", "http://LOCALHOST:8317"),
            ("gateway.example", "http://gateway.example"),
            ("gateway.example:80", "http://gateway.example"),
            ("gateway.example", "http://gateway.example:80"),
            // Behind a proxy that terminates TLS.
            ("gateway.example", "https://gateway.example"),
            ("gateway.example:443", "https://gateway.example"),
            ("[::1]:8317", "http://[::1]:8317"),
        ];
        for (host, origin) in same {
            assert!(
                !foreign(&[("host", host), ("origin", origin)]),
                "{origin} at {host}"
            );
        }
        let other = [
            ("127.0.0.1:8317", "https://evil.example"),
            ("127.0.0.1:8317", "http://127.0.0.1:3000"),
            ("127.0.0.1:8317", "http://localhost:8317"),
            ("gateway.example", "http://gateway.example:8080"),
            ("gateway.example:8317", "https://gateway.example"),
            ("gateway.example", "https://gateway.example.evil.test"),
            // Opaque and non-web origins are nobody's own.
            ("127.0.0.1:8317", "null"),
            ("127.0.0.1:8317", "file://"),
            ("127.0.0.1:8317", "app://obsidian.md"),
            ("127.0.0.1:8317", "chrome-extension://abcdefgh"),
            ("127.0.0.1:8317", ""),
            ("127.0.0.1:8317", "not an origin"),
        ];
        for (host, origin) in other {
            assert!(
                foreign(&[("host", host), ("origin", origin)]),
                "{origin} at {host}"
            );
        }
        // No host to compare with: nothing an origin could match.
        assert!(foreign(&[("origin", "http://127.0.0.1:8317")]));
        // One foreign origin among several is foreign.
        assert!(foreign(&[
            ("host", "127.0.0.1:8317"),
            ("origin", "http://127.0.0.1:8317"),
            ("origin", "https://evil.example"),
        ]));
    }

    #[test]
    fn the_host_of_an_http2_request_is_its_authority() {
        let (mut parts, ()) = http::Request::builder()
            .uri("https://gateway.example/v1/chat/completions")
            .header("origin", "https://gateway.example")
            .body(())
            .unwrap()
            .into_parts();
        assert!(!is_foreign_page(&parts));
        parts.headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://elsewhere.example"),
        );
        assert!(is_foreign_page(&parts));
    }

    #[test]
    fn the_browsers_own_verdict_comes_first() {
        // A page of this origin behind a proxy that rewrote `Host`.
        assert!(!foreign(&[
            ("host", "127.0.0.1:8317"),
            ("origin", "https://gateway.example"),
            ("sec-fetch-site", "same-origin"),
        ]));
        // The user's own request (address bar, bookmark).
        assert!(!foreign(&[("sec-fetch-site", "none")]));
        for verdict in ["cross-site", "same-site", "Cross-Site", "something-new"] {
            assert!(
                foreign(&[
                    ("host", "127.0.0.1:8317"),
                    ("origin", "http://127.0.0.1:8317"),
                    ("sec-fetch-site", verdict),
                ]),
                "{verdict}"
            );
        }
    }
}
