//! End-to-end tests of the real `switchyard` binary: a child process in a
//! temporary directory, on a port the operating system picks, talking to the
//! built-in mock provider. Nothing leaves the machine.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use switchyard_core::Config;

/// How long a child may take to print its `listening on` line (or to exit,
/// for the tests that expect it to). Start-up takes a fraction of a second;
/// the margin is for a machine that is busy compiling.
const DEADLINE: Duration = Duration::from_secs(30);

const SWITCHYARD_ENV: [&str; 4] = [
    "SWITCHYARD_CONFIG",
    "SWITCHYARD_ADMIN_SECRET",
    "SWITCHYARD_ADMIN_ALLOW_REMOTE",
    "SWITCHYARD_LOG",
];

/// The binary, with an environment that cannot leak into the test.
fn switchyard(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_switchyard"));
    command.current_dir(dir).stdin(Stdio::null());
    for name in SWITCHYARD_ENV {
        command.env_remove(name);
    }
    command
}

/// Runs a command that is expected to finish by itself.
fn run(dir: &Path, args: &[&str]) -> Output {
    switchyard(dir)
        .args(args)
        .output()
        .expect("the switchyard binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A running `switchyard serve`. Killed when dropped, so a failed assertion
/// never leaves a process behind.
struct Server {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn collect(stream: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let collected = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&collected);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => sink.lock().unwrap().push_str(&line),
            }
        }
    });
    collected
}

impl Server {
    fn start(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Server {
        let mut command = switchyard(dir);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("the switchyard binary starts");
        let stdout = collect(child.stdout.take().expect("piped stdout"));
        let stderr = collect(child.stderr.take().expect("piped stderr"));
        Server {
            child,
            stdout,
            stderr,
        }
    }

    fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Waits for the `listening on <url>` line and returns the URL.
    fn base_url(&mut self) -> String {
        let started = Instant::now();
        loop {
            if let Some(url) = self
                .stdout()
                .lines()
                .find_map(|line| line.strip_prefix("listening on ").map(str::to_string))
            {
                return url;
            }
            if let Some(status) = self.child.try_wait().expect("child status") {
                // Let the reader threads drain the pipes.
                std::thread::sleep(Duration::from_millis(100));
                panic!(
                    "switchyard exited ({status}) before listening.\nstderr:\n{}",
                    self.stderr()
                );
            }
            if started.elapsed() > DEADLINE {
                panic!(
                    "switchyard did not print its listening line within {DEADLINE:?}.\nstderr:\n{}",
                    self.stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the process to end by itself.
    fn exit_status(&mut self) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                // Let the reader threads drain the pipes.
                std::thread::sleep(Duration::from_millis(100));
                return status;
            }
            if started.elapsed() > DEADLINE {
                panic!(
                    "switchyard did not exit within {DEADLINE:?}.\nstderr:\n{}",
                    self.stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn http() -> reqwest::Client {
    // reqwest is built without a default TLS provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client")
}

fn read_config(path: &Path) -> Config {
    let text = std::fs::read_to_string(path).expect("the configuration file exists");
    switchyard_config_store::validate_text(&text).expect("the configuration file is valid")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// The text a Chat Completions stream delivered, from its SSE body.
fn streamed_text(body: &str) -> String {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn first_run_creates_the_config_and_serves() {
    let dir = tempfile::tempdir().unwrap();
    // A path with a space in it, and a directory that does not exist yet.
    let config_path = dir.path().join("my gateway").join("switchyard.toml");
    let mut server = Server::start(
        dir.path(),
        &["--config", config_path.to_str().unwrap(), "--port", "0"],
        &[],
    );
    let base = server.base_url();
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");
    assert!(!base.ends_with(":0"), "{base}");

    // The starter configuration was written, and announced once.
    let config = read_config(&config_path);
    let admin_secret = config.admin.secret.clone();
    let client_key = config.auth.keys[0].key.clone();
    assert_eq!(admin_secret.len(), 32);
    assert!(client_key.starts_with("sy-") && client_key.len() == 43);
    let banner = server.stderr();
    assert!(banner.contains("First run"), "{banner}");
    assert_eq!(banner.matches(&admin_secret).count(), 1, "{banner}");
    assert_eq!(banner.matches(&client_key).count(), 1, "{banner}");
    assert!(banner.contains("is running"), "{banner}");
    assert!(banner.contains(&format!("listening  {base}")), "{banner}");
    assert!(
        banner.contains(&format!("dashboard  {base}/admin/")),
        "{banner}"
    );
    assert!(banner.contains("my gateway"), "{banner}");
    assert!(banner.contains("1 provider, 1 credential"), "{banner}");
    assert!(
        banner.contains("only the built-in mock provider"),
        "{banner}"
    );
    // stdout carries the machine-readable line and nothing else.
    assert_eq!(server.stdout(), format!("listening on {base}\n"));

    let http = http();

    let health = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    let health: serde_json::Value = health.json().await.unwrap();
    assert_eq!(health["status"], "ok");

    // Chat Completions against the mock provider, with the generated key.
    let request = serde_json::json!({
        "model": "mock-echo",
        "messages": [{"role": "user", "content": "hello switchyard"}]
    });
    let reply = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&client_key)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let reply: serde_json::Value = reply.json().await.unwrap();
    let text = reply["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("hello switchyard"), "{reply}");

    // The same, streaming.
    let mut streaming = request.clone();
    streaming["stream"] = true.into();
    let reply = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&client_key)
        .json(&streaming)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let content_type = reply.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    let body = reply.text().await.unwrap();
    assert!(streamed_text(&body).contains("hello switchyard"), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");

    // No key, no service.
    let refused = http
        .post(format!("{base}/v1/chat/completions"))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);

    // The dashboard and the admin API, with the generated secret.
    let dashboard = http.get(format!("{base}/admin/")).send().await.unwrap();
    assert_eq!(dashboard.status(), 200);
    let content_type = dashboard.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert!(dashboard.text().await.unwrap().contains("<html"));

    let status = http
        .get(format!("{base}/admin/api/status"))
        .bearer_auth(&admin_secret)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let status: serde_json::Value = status.json().await.unwrap();
    assert!(status.is_object(), "{status}");

    let wrong = http
        .get(format!("{base}/admin/api/status"))
        .bearer_auth("not-the-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    // Requests made it into the application log, which goes to stderr as
    // JSON lines when stderr is not a terminal.
    let log = server.stderr();
    assert!(
        log.lines()
            .any(|line| line.starts_with('{') && line.contains("\"level\":\"INFO\"")),
        "{log}"
    );
}

#[tokio::test]
async fn a_second_start_uses_the_existing_config_and_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let init = run(dir.path(), &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));
    let config_path = dir.path().join("switchyard.toml");
    let config = read_config(&config_path);
    let before = std::fs::read_to_string(&config_path).unwrap();

    // The configuration is found through SWITCHYARD_CONFIG; the admin
    // secret comes from the environment; the log filter too.
    let mut server = Server::start(
        dir.path(),
        &["serve", "--port", "0", "--host", "127.0.0.1"],
        &[
            ("SWITCHYARD_CONFIG", config_path.to_str().unwrap()),
            ("SWITCHYARD_ADMIN_SECRET", "secret-from-the-environment"),
            ("SWITCHYARD_LOG", "warn"),
        ],
    );
    let base = server.base_url();
    let banner = server.stderr();
    assert!(!banner.contains("First run"), "{banner}");
    assert!(!banner.contains(&config.admin.secret), "{banner}");
    assert!(!banner.contains(&config.auth.keys[0].key), "{banner}");
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);

    let http = http();
    for (secret, expected) in [
        ("secret-from-the-environment", 200),
        (config.admin.secret.as_str(), 401),
    ] {
        let status = http
            .get(format!("{base}/admin/api/status"))
            .bearer_auth(secret)
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), expected);
    }
    let models = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(&config.auth.keys[0].key)
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let models: serde_json::Value = models.json().await.unwrap();
    assert!(
        models["data"]
            .as_array()
            .is_some_and(|list| list.iter().any(|model| model["id"] == "mock-echo")),
        "{models}"
    );

    // At `warn` nothing is logged about an uneventful start.
    let log = server.stderr();
    assert!(!log.contains("\"level\":\"INFO\""), "{log}");
}

#[test]
fn an_invalid_config_makes_serve_exit_2_with_every_issue() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("switchyard.toml");
    std::fs::write(
        &config_path,
        "[server]\nport = 0\n\n[[providers]]\nname = \"Bad Name\"\nkind = \"openai-compat\"\n",
    )
    .unwrap();
    let mut server = Server::start(dir.path(), &["serve", "--port", "0"], &[]);
    let status = server.exit_status();
    assert_eq!(status.code(), Some(2), "{}", server.stderr());
    let errors = server.stderr();
    assert!(errors.starts_with("error: "), "{errors}");
    for issue in [
        "\n  server.port: must be between 1 and 65535",
        "\n  providers[0].name: ",
        "\n  providers[0].base_url: ",
    ] {
        assert!(errors.contains(issue), "{errors}");
    }
    assert!(!errors.contains("panicked"), "{errors}");
    assert_eq!(server.stdout(), "");

    // The same file through `check`.
    let check = run(dir.path(), &["check"]);
    assert_eq!(check.status.code(), Some(2));
    let errors = stderr(&check);
    assert!(
        errors.contains("\n  server.port: must be between 1 and 65535"),
        "{errors}"
    );
    assert_eq!(stdout(&check), "");
}

#[test]
fn a_port_in_use_is_named_with_the_way_out() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut server = Server::start(dir.path(), &["--port", &port.to_string()], &[]);
    let status = server.exit_status();
    assert_eq!(status.code(), Some(1), "{}", server.stderr());
    let errors = server.stderr();
    assert!(
        errors.contains(&format!("port {port} is already in use")),
        "{errors}"
    );
    assert!(errors.contains("--port"), "{errors}");
    assert!(!errors.contains("panicked"), "{errors}");
    assert!(!server.stdout().contains("listening on"));
    drop(taken);
}

#[test]
fn init_check_version_and_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("sub dir").join("gateway.toml");
    let config_arg = config_path.to_str().unwrap();

    let init = run(dir.path(), &["init", "--config", config_arg]);
    assert_eq!(init.status.code(), Some(0), "{}", stderr(&init));
    let config = read_config(&config_path);
    let printed = stdout(&init);
    assert!(printed.contains("gateway.toml"), "{printed}");
    assert!(printed.contains(&config.admin.secret), "{printed}");
    assert!(printed.contains(&config.auth.keys[0].key), "{printed}");
    assert!(
        printed.contains("http://127.0.0.1:8317/admin/"),
        "{printed}"
    );
    assert_eq!(stderr(&init), "");

    // A second init refuses, and leaves the file alone.
    let before = std::fs::read_to_string(&config_path).unwrap();
    let again = run(dir.path(), &["init", "--config", config_arg]);
    assert_eq!(again.status.code(), Some(2));
    assert!(stderr(&again).contains("--force"), "{}", stderr(&again));
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
    let forced = run(dir.path(), &["init", "--config", config_arg, "--force"]);
    assert_eq!(forced.status.code(), Some(0), "{}", stderr(&forced));
    assert_ne!(std::fs::read_to_string(&config_path).unwrap(), before);

    // check: by flag, and by environment variable.
    let check = run(dir.path(), &["check", "--config", config_arg]);
    assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));
    let report = stdout(&check);
    assert!(report.starts_with("ok: "), "{report}");
    assert!(report.contains("providers    1"), "{report}");
    let config = read_config(&config_path);
    assert!(!report.contains(&config.admin.secret), "{report}");
    assert!(!report.contains(&config.auth.keys[0].key), "{report}");
    let by_env = switchyard(dir.path())
        .arg("check")
        .env("SWITCHYARD_CONFIG", config_arg)
        .output()
        .unwrap();
    assert_eq!(by_env.status.code(), Some(0), "{}", stderr(&by_env));
    assert_eq!(stdout(&by_env), report);

    // Without either, ./switchyard.toml is looked for (and is not there).
    let missing = run(dir.path(), &["check"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(
        stderr(&missing).contains("switchyard init"),
        "{}",
        stderr(&missing)
    );

    for args in [&["version"][..], &["--version"], &["-V"]] {
        let version = run(dir.path(), args);
        assert_eq!(version.status.code(), Some(0));
        assert_eq!(
            stdout(&version).trim(),
            format!("switchyard {}", env!("CARGO_PKG_VERSION"))
        );
    }

    let help = run(dir.path(), &["--help"]);
    assert_eq!(help.status.code(), Some(0));
    let help = stdout(&help);
    for needle in [
        "import-cliproxy",
        "SWITCHYARD_CONFIG",
        "SWITCHYARD_ADMIN_SECRET",
        "SWITCHYARD_ADMIN_ALLOW_REMOTE",
        "SWITCHYARD_LOG",
    ] {
        assert!(help.contains(needle), "{help}");
    }

    for args in [
        &["--no-such-flag"][..],
        &["serve", "--port", "99999"],
        &["explode"],
    ] {
        let usage = run(dir.path(), args);
        assert_eq!(usage.status.code(), Some(2), "{args:?}");
        assert_eq!(stdout(&usage), "");
        assert!(!stderr(&usage).is_empty());
    }
}

#[test]
fn import_cliproxy_converts_both_layouts() {
    let dir = tempfile::tempdir().unwrap();
    for (name, secrets) in [
        (
            "cliproxy-flat.yaml",
            &[
                "flat-management-secret",
                "sk-client-flat-0001",
                "sk-or-flat-0000000000000000001",
            ][..],
        ),
        (
            "cliproxy-v8.yaml",
            &[
                "sk-client-v8-0001",
                "sk-ant-v8-00000000000000000000001",
                "xai-v8-key-000000000000000000001",
            ],
        ),
    ] {
        let input = fixture(name);
        let output = dir.path().join(format!("{name}.toml"));
        let import = run(
            dir.path(),
            &[
                "import-cliproxy",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
            ],
        );
        assert_eq!(import.status.code(), Some(0), "{}", stderr(&import));
        let summary = stdout(&import);
        let report = stderr(&import);
        assert!(summary.starts_with("imported "), "{summary}");
        assert!(report.starts_with("not imported:\n"), "{report}");
        for secret in secrets {
            assert!(!summary.contains(secret), "{secret} in:\n{summary}");
            assert!(!report.contains(secret), "{secret} in:\n{report}");
        }

        // The result is a configuration `check` accepts, with the report in
        // its header.
        let config = read_config(&output);
        assert!(!config.providers.is_empty());
        let written = std::fs::read_to_string(&output).unwrap();
        assert!(written.contains("# Not imported:\n"), "{written}");
        let check = run(dir.path(), &["check", "--config", output.to_str().unwrap()]);
        assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));

        // No overwriting without --force.
        let again = run(
            dir.path(),
            &[
                "import-cliproxy",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
            ],
        );
        assert_eq!(again.status.code(), Some(2));
        assert!(stderr(&again).contains("--force"));
    }

    let missing = run(dir.path(), &["import-cliproxy", "nope.yaml"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(!dir.path().join("switchyard.toml").exists());
}

/// Keys and passwords written without quotes are imported as the file has
/// them, and a file built to exhaust memory is refused with a sentence.
#[test]
fn import_cliproxy_keeps_unquoted_secrets_and_refuses_alias_bombs() {
    let dir = tempfile::tempdir().unwrap();

    let input = dir.path().join("numeric.yaml");
    std::fs::write(
        &input,
        "port: 8317\n\
         remote-management:\n  secret-key: 012345\n\
         api-keys:\n  - 00998877\n  - 123456789012345678901234567890\n  - 0x1F\n\
         gemini-api-key:\n  - api-key: 0123456789\n",
    )
    .unwrap();
    let output = dir.path().join("numeric.toml");
    let import = run(
        dir.path(),
        &[
            "import-cliproxy",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ],
    );
    assert_eq!(import.status.code(), Some(0), "{}", stderr(&import));
    let config = read_config(&output);
    assert_eq!(config.admin.secret, "012345");
    let keys: Vec<&str> = config.auth.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["00998877", "123456789012345678901234567890", "0x1F"]);
    assert_eq!(config.providers[0].api_keys, ["0123456789"]);
    let printed = format!("{}{}", stdout(&import), stderr(&import));
    for secret in ["012345", "00998877", "0123456789"] {
        assert!(!printed.contains(secret), "{secret} in:\n{printed}");
    }

    // 70 KiB that would expand to 64 MiB; at the size limit of the importer
    // the same shape would ask for hundreds of gigabytes.
    let bomb = dir.path().join("bomb.yaml");
    let mut text = format!("big: &big \"{}\"\ncopies:\n", "k".repeat(64 * 1024));
    for _ in 0..1000 {
        text.push_str("  - *big\n");
    }
    std::fs::write(&bomb, text).unwrap();
    let refused_output = dir.path().join("bomb.toml");
    let refused = run(
        dir.path(),
        &[
            "import-cliproxy",
            bomb.to_str().unwrap(),
            "-o",
            refused_output.to_str().unwrap(),
        ],
    );
    assert_eq!(refused.status.code(), Some(2));
    let message = stderr(&refused);
    assert!(
        message.contains("the document expands to more than 16 MiB of text"),
        "{message}"
    );
    assert!(message.starts_with("error: "), "{message}");
    assert!(!message.contains("kkkk") && !message.contains("panicked"));
    assert!(message.lines().count() <= 2, "{message}");
    assert!(!refused_output.exists());
}

/// SIGTERM is how `docker stop` and service managers ask a process to end.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_shuts_down_gracefully_with_exit_code_0() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(dir.path(), &["--port", "0"], &[]);
    let base = server.base_url();
    let health = http().get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    let killed = Command::new("kill")
        .args(["-TERM", &server.child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    let status = server.exit_status();
    assert_eq!(status.code(), Some(0), "{}", server.stderr());
    let log = server.stderr();
    assert!(log.contains("shutting down"), "{log}");
    assert!(log.contains("stopped"), "{log}");
}
