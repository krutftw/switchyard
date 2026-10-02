//! Review findings in the upstream WebSocket client.
//!
//! These started as failing tests left by the adversarial review (findings
//! UP-7, UP-8, UP-9) and are kept as regression tests: the doc comment of each test
//! describes the defect as it was found, the assertions the behaviour that
//! is now implemented.

mod common;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use common::{client, serve, target};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use switchyard_core::Protocol;
use switchyard_core::config::ProviderKind;
use switchyard_upstream::Target;

/// What the upstream saw on the handshake request.
#[derive(Clone, Default)]
struct Seen {
    protocol_lines: Arc<Mutex<Vec<String>>>,
    user_agent_lines: Arc<Mutex<Vec<String>>>,
}

fn lines(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_string)
        .collect()
}

/// Selects the subprotocol `beta` — one the client offered.
async fn upstream(State(seen): State<Seen>, ws: WebSocketUpgrade, headers: HeaderMap) -> Response {
    *seen.protocol_lines.lock().unwrap() = lines(&headers, "sec-websocket-protocol");
    *seen.user_agent_lines.lock().unwrap() = lines(&headers, "user-agent");
    ws.protocols(["beta"])
        .on_upgrade(|mut socket: WebSocket| async move {
            // Hold the socket until the client goes away.
            let _ = socket.recv().await;
        })
}

async fn server(seen: Seen) -> SocketAddr {
    serve(
        Router::new()
            .route("/v1/realtime", get(upstream))
            .with_state(seen),
    )
    .await
}

fn ws_target(addr: SocketAddr) -> Target {
    target(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        format!("http://{addr}/v1"),
        "gpt-test",
    )
}

/// RFC 6455 §4.1 / §11.3.4: `Sec-WebSocket-Protocol` "MAY appear multiple
/// times in an HTTP request (which is logically the same as a single
/// header field that contains all values)". A relay that hands the client's
/// header map to `connect_ws` therefore may pass two lines.
///
/// `build_ws_request` *appends* each line as its own header. tungstenite
/// only reads the first line when it records which subprotocols were
/// offered, so when the upstream selects one from the second line the
/// handshake is rejected locally ("Server sent an invalid subprotocol")
/// although the upstream accepted the connection.
#[tokio::test]
async fn subprotocols_offered_on_separate_header_lines_can_be_selected() {
    let seen = Seen::default();
    let addr = server(seen.clone()).await;

    let mut extra = HeaderMap::new();
    extra.append("sec-websocket-protocol", "alpha".parse().unwrap());
    extra.append("sec-websocket-protocol", "beta".parse().unwrap());
    let connection = client()
        .connect_ws_with(
            &ws_target(addr),
            "realtime?model=gpt-realtime",
            &extra,
            Duration::from_secs(5),
        )
        .await;

    // The upstream was offered both and picked `beta`.
    let offered = seen.protocol_lines.lock().unwrap().join(", ");
    assert!(
        offered.contains("alpha") && offered.contains("beta"),
        "{offered}"
    );
    let connection = connection.unwrap_or_else(|e| {
        panic!(
            "the handshake the upstream accepted was rejected locally: {}",
            e.info.message
        )
    });
    assert_eq!(
        connection
            .headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("beta")
    );
}

/// `build_ws_request` installs `user-agent: switchyard/<version>` and then
/// appends the caller's `user-agent` (notes 04 §2.2 list `User-Agent` among
/// the headers a relay passes through), so the handshake carries two
/// `User-Agent` header lines. `User-Agent` is a singleton field (RFC 9110
/// §5.3 allows repeating only list-valued fields); exactly one must be sent.
#[tokio::test]
async fn the_handshake_carries_a_single_user_agent() {
    let seen = Seen::default();
    let addr = server(seen.clone()).await;

    let mut extra = HeaderMap::new();
    extra.insert("sec-websocket-protocol", "beta".parse().unwrap());
    extra.insert("user-agent", "relayed-client/1.0".parse().unwrap());
    client()
        .connect_ws_with(
            &ws_target(addr),
            "realtime?model=gpt-realtime",
            &extra,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

    let agents = seen.user_agent_lines.lock().unwrap().clone();
    assert_eq!(
        agents.len(),
        1,
        "the upstream received {} User-Agent header lines: {agents:?}",
        agents.len()
    );
}

/// DESIGN "Rules for everyone": panics are bugs. `connect_ws_with` computes
/// `Instant::now() + connect_timeout`, which panics for a very large
/// timeout ("overflow when adding duration to instant"). A caller that
/// means "no limit" (`Duration::MAX`) takes the task down instead of
/// connecting.
#[tokio::test]
async fn a_huge_connect_timeout_does_not_panic() {
    let seen = Seen::default();
    let addr = server(seen).await;
    let mut extra = HeaderMap::new();
    extra.insert("sec-websocket-protocol", "beta".parse().unwrap());

    let outcome = tokio::spawn(async move {
        client()
            .connect_ws_with(
                &ws_target(addr),
                "realtime?model=gpt-realtime",
                &extra,
                Duration::MAX,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.info.message)
    })
    .await;
    let outcome = outcome.expect("connect_ws_with panicked");
    assert_eq!(outcome, Ok(()));
}

async fn plain_ok() -> &'static str {
    "{}"
}

/// The HTTP path (reqwest / hyper-util) dials a multi-address host with
/// "happy eyeballs": when the first address does not answer promptly the
/// next one is tried in parallel. The WebSocket path dials by hand with
/// `TcpStream::connect((host, port))`, which walks the resolved addresses
/// strictly one after the other, each for as long as the operating system
/// takes to give up — all inside the single connect deadline.
///
/// So a host whose first address is dead (an unreachable AAAA record on a
/// network with broken IPv6, or simply `localhost` on a machine where the
/// server listens on 127.0.0.1 only and `::1` is tried first) makes the
/// upstream WebSocket slow or — within the connect timeout — impossible,
/// while HTTP calls to the very same base URL succeed.
///
/// Platform note: the test needs `localhost` to resolve to `::1` *before*
/// `127.0.0.1` and the refused `::1` attempt to be slow (Windows retries a
/// refused connect for about two seconds). Where that is not the case the
/// test cannot observe the defect and returns early.
#[tokio::test]
async fn a_dead_first_address_does_not_stop_the_websocket_from_connecting() {
    use axum::body::Bytes;
    use switchyard_upstream::{Operation, Timeouts};

    let seen = Seen::default();
    let addr = serve(
        Router::new()
            .route("/v1/realtime", get(upstream).post(plain_ok))
            .with_state(seen),
    )
    .await;
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host(("localhost", addr.port()))
        .await
        .map(|addrs| addrs.collect())
        .unwrap_or_default();
    let v6_first = resolved.first().is_some_and(SocketAddr::is_ipv6)
        && resolved.iter().any(SocketAddr::is_ipv4);
    if !v6_first {
        eprintln!("skipped: localhost resolves to {resolved:?}");
        return;
    }
    // How long does the dead first address take to fail on this platform?
    let probe = std::time::Instant::now();
    let _ = tokio::net::TcpStream::connect(resolved[0]).await;
    if probe.elapsed() < Duration::from_millis(1200) {
        eprintln!(
            "skipped: the refused connect only took {:?}",
            probe.elapsed()
        );
        return;
    }

    let target = target(
        ProviderKind::Openai,
        Protocol::OpenaiResponses,
        format!("http://localhost:{}/v1", addr.port()),
        "gpt-test",
    );
    let budget = Duration::from_secs(1);
    let client = client();

    // Control: an HTTP call with the same one-second connect budget works.
    client
        .send(
            &target,
            &Operation::Raw {
                method: http::Method::POST,
                path: "realtime".into(),
                query: None,
            },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            Timeouts {
                connect: budget,
                request: Duration::from_secs(10),
            },
        )
        .await
        .expect("the HTTP call reaches the server within the connect budget");

    let mut extra = HeaderMap::new();
    extra.insert("sec-websocket-protocol", "beta".parse().unwrap());
    let outcome = client
        .connect_ws_with(&target, "realtime?model=gpt-realtime", &extra, budget)
        .await;
    if let Err(error) = outcome {
        panic!(
            "HTTP reached localhost:{} within {budget:?} but the WebSocket did not: {}",
            addr.port(),
            error.info.message
        );
    }
}
