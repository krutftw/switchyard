//! Regression test (review finding SY-BIN-1): at `logging.level = "trace"`
//! the log subscriber must not feed on its own output when a dashboard is
//! connected.
//!
//! The capture layer publishes every log line on the event bus; the admin
//! live-event WebSocket sends every published line to the dashboard; and at
//! `trace` the WebSocket library's own targets (`tungstenite`,
//! `tokio_tungstenite`) are no longer capped, so *sending* a log line logs
//! about a dozen new lines. When those were captured too, each of them was
//! published and sent in turn, and an idle gateway wrote tens of thousands
//! of lines per second to stderr (and to the log files and the dashboard)
//! until the dashboard was closed. "Trace" is an option of the dashboard's
//! Settings page, so the page that triggered the storm was by definition
//! open when it started.
//!
//! The test does what the dashboard does: sets the level through
//! `PATCH /admin/api/settings`, opens the live-event WebSocket, and then
//! watches an otherwise idle gateway. It passes because the lines the
//! delivery libraries write below `warn` are kept out of the capture layer
//! (`switchyard::logging`); stderr still shows them, about twenty a second
//! for the one `stats` frame the socket carries.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(30);

/// A running `switchyard serve`, killed when dropped.
struct Server {
    child: Child,
    stdout: Arc<Mutex<String>>,
    /// Number of lines the child has written to stderr so far.
    stderr_lines: Arc<Mutex<usize>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(dir: &Path) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_switchyard"));
        command
            .current_dir(dir)
            .args(["--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "SWITCHYARD_CONFIG",
            "SWITCHYARD_ADMIN_SECRET",
            "SWITCHYARD_ADMIN_ALLOW_REMOTE",
            "SWITCHYARD_LOG",
        ] {
            command.env_remove(name);
        }
        let mut child = command.spawn().expect("the switchyard binary starts");

        let stdout = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stdout);
        let out = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || {
            let mut reader = BufReader::new(out);
            let mut line = String::new();
            while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                sink.lock().unwrap().push_str(&line);
                line.clear();
            }
        });

        // Only counted: a storm would otherwise be megabytes of text.
        let stderr_lines = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&stderr_lines);
        let err = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            let mut reader = BufReader::new(err);
            let mut buffer = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let lines = buffer[..n].iter().filter(|b| **b == b'\n').count();
                        *counter.lock().unwrap() += lines;
                    }
                }
            }
        });

        Server {
            child,
            stdout,
            stderr_lines,
        }
    }

    fn base_url(&mut self) -> String {
        let started = Instant::now();
        loop {
            if let Some(url) = self
                .stdout
                .lock()
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("listening on ").map(str::to_string))
            {
                return url;
            }
            assert!(
                self.child.try_wait().expect("child status").is_none(),
                "switchyard exited before listening"
            );
            assert!(started.elapsed() < DEADLINE, "no listening line");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stderr_lines(&self) -> usize {
        *self.stderr_lines.lock().unwrap()
    }
}

/// Opens the admin live-event WebSocket by hand (no WebSocket client is
/// among the dev-dependencies) and returns the upgraded connection.
fn open_live_socket(base: &str, ticket: &str) -> TcpStream {
    let authority = base.strip_prefix("http://").expect("plain HTTP");
    let mut stream = TcpStream::connect(authority).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        stream,
        "GET /admin/api/ws?ticket={ticket} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Origin: {base}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    )
    .expect("the handshake is sent");

    // Read up to the end of the response head.
    let started = Instant::now();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert!(started.elapsed() < DEADLINE, "no handshake response");
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            Ok(_) => panic!("the server closed the connection during the handshake"),
            Err(_) => continue,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the live-event WebSocket was not opened:\n{head}"
    );
    stream
}

/// Keeps reading (and discarding) what the server sends for `duration`, so
/// that the socket never fills up.
fn drain(stream: &mut TcpStream, duration: Duration) {
    let started = Instant::now();
    let mut buffer = [0u8; 64 * 1024];
    while started.elapsed() < duration {
        // A timeout only means that nothing was sent for a moment.
        if let Ok(0) = stream.read(&mut buffer) {
            panic!("the server closed the live-event WebSocket");
        }
    }
}

#[tokio::test]
async fn trace_level_with_a_dashboard_connected_does_not_log_about_its_own_log_lines() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(dir.path());
    let base = server.base_url();

    let text = std::fs::read_to_string(dir.path().join("switchyard.toml")).unwrap();
    let config = switchyard_config_store::validate_text(&text).expect("starter config");
    let secret = config.admin.secret.clone();

    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();

    // What the dashboard's Settings page does for "Trace".
    let patched = http
        .patch(format!("{base}/admin/api/settings"))
        .bearer_auth(&secret)
        .json(&serde_json::json!({"logging": {"level": "trace"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200);

    // What every dashboard page does: open the live-event stream.
    let ticket: serde_json::Value = http
        .post(format!("{base}/admin/api/ws-ticket"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ticket = ticket["ticket"].as_str().expect("a ticket").to_string();
    let mut socket = open_live_socket(&base, &ticket);

    // Let the handshake's own trace output pass, then watch a gateway that
    // has nothing to do: no request arrives, and the only thing the socket
    // carries by itself is one `stats` frame a second.
    drain(&mut socket, Duration::from_millis(1000));
    let before = server.stderr_lines();
    drain(&mut socket, Duration::from_millis(1500));
    let after = server.stderr_lines();
    let written = after - before;

    assert!(
        written < 2000,
        "an idle gateway wrote {written} log lines to stderr in 1.5 s at trace level with one \
         dashboard connected: delivering a log line to the dashboard logs new lines, which are \
         delivered in turn"
    );
}
