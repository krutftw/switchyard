//! Regression tests for review finding ADM-R1 (fixed): the sign-in lockout
//! was keyed by the TCP peer address alone, so everybody a local reverse
//! proxy relays shared one bucket with the operator who connects to the
//! gateway directly on loopback.
//!
//! The guard classifies a loopback peer that carries `X-Forwarded-For` /
//! `Forwarded` / `X-Real-IP` as REMOTE ("a reverse proxy on this machine
//! relaying somebody else"). The lockout used to file that remote client's
//! wrong guesses under `127.0.0.1` — the address of the genuinely local
//! operator. With `admin.allow_remote` on and a proxy in front (the
//! deployment the forwarding rule exists for), any unauthenticated client on
//! the internet could keep the admin API locked for everyone, including the
//! operator on the machine itself, with five requests every thirty minutes.
//!
//! Now: failures of relayed requests are counted in a bucket of their own
//! (`auth::LockoutKey`) and are never held against a direct loopback
//! connection, nor the other way round. The address a proxy reports is
//! deliberately not part of the bucket (a client can send the header too),
//! and on a connection that is not loopback the header changes nothing.

mod support;

use http::{Method, StatusCode};
use support::{App, BASE};

fn allow_remote() -> String {
    BASE.replace("[admin]", "[admin]\nallow_remote = true")
}

#[tokio::test]
async fn relayed_failures_do_not_lock_out_the_operator_on_loopback() {
    let app = App::start_config(&allow_remote()).await;

    // Somebody on the internet, relayed by the reverse proxy on this
    // machine (direct peer 127.0.0.1, `X-Forwarded-For` set), guesses the
    // secret five times.
    let mut statuses = Vec::new();
    for _ in 0..5 {
        let response = app
            .http
            .get(app.api("/status"))
            .header("x-forwarded-for", "203.0.113.9")
            .bearer_auth("not-the-secret")
            .send()
            .await
            .unwrap();
        statuses.push(response.status());
    }
    assert_eq!(
        statuses,
        [
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS,
        ]
    );

    // The guesser stays locked out (a fix must not simply stop counting
    // relayed failures).
    let relayed = app
        .http
        .get(app.api("/status"))
        .header("x-forwarded-for", "203.0.113.9")
        .bearer_auth(&app.secret)
        .send()
        .await
        .unwrap();
    assert_eq!(relayed.status(), StatusCode::TOO_MANY_REQUESTS);

    // The operator on the machine itself, connecting directly (no
    // forwarding header: this is not a relayed request), presents the right
    // secret. Nothing this connection did was wrong.
    let direct = app.request(Method::GET, "/status").send().await.unwrap();
    assert_eq!(
        direct.status(),
        StatusCode::OK,
        "a remote client relayed by the local proxy locked the local operator out"
    );
}

#[tokio::test]
async fn relayed_failures_do_not_lock_out_the_operator_when_remote_access_is_refused_later() {
    // Same thing seen from the recovery path: the operator turns remote
    // access off (by editing the file) to stop the guessing, and still
    // cannot sign in locally, because the bucket the remote guesses were
    // counted in is the operator's own address.
    let app = App::start_config(&allow_remote()).await;
    for _ in 0..5 {
        let _ = app
            .http
            .get(app.api("/status"))
            .header("forwarded", "for=198.51.100.4")
            .bearer_auth("not-the-secret")
            .send()
            .await
            .unwrap();
    }
    std::fs::write(&app.config_path, BASE).unwrap();
    app.gateway.config_store().reload_from_disk().await.unwrap();
    support::eventually(|| (!app.gateway.config().admin.allow_remote).then_some(())).await;

    let direct = app.request(Method::GET, "/status").send().await.unwrap();
    assert_eq!(
        direct.status(),
        StatusCode::OK,
        "the local operator is locked out by guesses that were relayed from elsewhere"
    );
}

#[tokio::test]
async fn a_locked_out_direct_connection_does_not_lock_what_the_proxy_relays() {
    // The other direction: something on the machine itself guesses wrong
    // five times. People who come in through the proxy are not held to
    // account for it.
    let app = App::start_config(&allow_remote()).await;
    for _ in 0..5 {
        let _ = app
            .http
            .get(app.api("/status"))
            .bearer_auth("not-the-secret")
            .send()
            .await
            .unwrap();
    }
    let direct = app.request(Method::GET, "/status").send().await.unwrap();
    assert_eq!(direct.status(), StatusCode::TOO_MANY_REQUESTS);

    for (name, value) in [
        ("x-forwarded-for", "203.0.113.9"),
        ("forwarded", "for=203.0.113.9"),
        ("x-real-ip", "203.0.113.9"),
    ] {
        let relayed = app
            .request(Method::GET, "/status")
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(relayed.status(), StatusCode::OK, "{name}");
    }
}

#[tokio::test]
async fn everything_the_proxy_relays_shares_one_count_whatever_address_it_reports() {
    // The address in the header is the client's to choose unless the proxy
    // overwrites it, so it must not select the bucket: a guesser who sends
    // a new one with every attempt is locked out all the same.
    let app = App::start_config(&allow_remote()).await;
    let mut statuses = Vec::new();
    for (name, value) in [
        ("x-forwarded-for", "203.0.113.1"),
        ("x-forwarded-for", "203.0.113.2, 198.51.100.9"),
        ("forwarded", "for=203.0.113.3"),
        ("x-real-ip", "203.0.113.4"),
        ("x-forwarded-for", "203.0.113.5"),
    ] {
        let response = app
            .http
            .get(app.api("/status"))
            .header(name, value)
            .bearer_auth("not-the-secret")
            .send()
            .await
            .unwrap();
        statuses.push(response.status().as_u16());
    }
    assert_eq!(statuses, [401, 401, 401, 401, 429]);
}

#[tokio::test]
async fn a_forwarding_header_buys_a_remote_peer_no_second_set_of_guesses() {
    // On a connection that is not loopback nothing vouches for the header:
    // with it or without, the failures are the peer's.
    let app = App::start_config(&allow_remote()).await;
    let peer = "203.0.113.50:4000";
    let mut statuses = Vec::new();
    for attempt in 0..5 {
        let mut request = app
            .http
            .get(app.api("/status"))
            .header("x-test-peer", peer)
            .bearer_auth("not-the-secret");
        if attempt % 2 == 1 {
            request = request.header("x-forwarded-for", "10.0.0.1");
        }
        statuses.push(request.send().await.unwrap().status().as_u16());
    }
    assert_eq!(statuses, [401, 401, 401, 401, 429]);
    for with_header in [false, true] {
        let mut request = app
            .request(Method::GET, "/status")
            .header("x-test-peer", peer);
        if with_header {
            request = request.header("x-forwarded-for", "10.0.0.1");
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
