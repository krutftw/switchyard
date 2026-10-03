//! `switchyard serve`: start the gateway and run until told to stop.

use crate::banner::{self, Dashboard, StartInfo};
use crate::check::read_config_text;
use crate::cli::{self, LOG_ENV, ServeArgs};
use crate::error::{CliError, EXIT_FAILURE};
use crate::logging::{self, LogControl};
use crate::out;
use crate::starter::{self, STARTER_HOST, absolute};
use crate::urls;
use std::io::{ErrorKind, IsTerminal};
use std::path::Path;
use std::time::Duration;
use switchyard_admin::AdminOptions;
use switchyard_config_store::{ConfigStoreError, validate_text};
use switchyard_core::config::DEFAULT_PORT;
use switchyard_gateway::{Gateway, GatewayOptions, StartError};
use switchyard_server::ServeOptions;

/// How long the runtime may take to wind down after the server has
/// drained and the gateway has flushed.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(5);

/// Runs the gateway until Ctrl-C (or SIGTERM), then shuts down gracefully.
pub fn run(args: &ServeArgs) -> Result<(), CliError> {
    let path = cli::config_path(args.config.as_deref());

    // First run: write a starter configuration and say what is in it.
    if !path.exists() {
        // The file says which flags this run used, so the address in its
        // comments is the one in use, and the values in it are not taken
        // for it.
        let flags = starter::Flags {
            host: args.host.clone(),
            port: args.port,
        };
        let created = starter::create_for(&path, false, &flags)?;
        let host = args.host.as_deref().unwrap_or(STARTER_HOST);
        let dashboard = match args.port.unwrap_or(DEFAULT_PORT) {
            // The port is only known once the listener is bound; the start
            // banner names it.
            0 => None,
            port => Some(urls::dashboard_url(&urls::base_url(host, port, false))),
        };
        out::stderr_line(&format!(
            "First run: {}\n",
            created
                .announcement_with_secrets(dashboard.as_deref(), std::io::stderr().is_terminal())
        ));
    }

    // Read once before the gateway starts: for the log level, and to refuse
    // an invalid file with every issue listed.
    let text = read_config_text(&path)?;
    let config = validate_text(&text).map_err(|issues| CliError::invalid_config(&path, &issues))?;

    // Both TLS stacks (listener and upstream) take the process default. An
    // error only means a provider is installed already.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let env_filter = std::env::var(LOG_ENV).ok();
    let (log, problem) = logging::init(&config.logging.level, env_filter.as_deref());
    if let Some(problem) = problem {
        out::stderr_line(&format!("warning: {problem}"));
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| CliError::failure(format!("cannot start the async runtime: {error}")))?;
    let result = runtime.block_on(serve(&path, args, log));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    result
}

async fn serve(path: &Path, args: &ServeArgs, log: LogControl) -> Result<(), CliError> {
    let gateway = Gateway::start(GatewayOptions::new(path))
        .await
        .map_err(|error| start_error(path, error))?;

    // From here on log lines also reach the dashboard, and the level
    // follows the configuration.
    log.attach_telemetry(gateway.telemetry());
    gateway.on_log_level(move |level| log.set_level(level));

    let config = gateway.config();
    let mut options = ServeOptions::from_config(&config, gateway.config_store().dir());
    if let Some(host) = &args.host {
        options.host = host.clone();
    }
    if let Some(port) = args.port {
        options.port = port;
    }
    let (host, port, tls) = (options.host.clone(), options.port, options.tls.is_some());

    let bound = match switchyard_server::bind(options).await {
        Ok(bound) => bound,
        Err(error) => {
            gateway.shutdown().await;
            return Err(bind_error(&host, port, &error));
        }
    };
    let addr = bound.local_addr();

    let mut admin = AdminOptions::from_env();
    admin.listen = Some(addr);
    admin.tls = tls;
    admin.command_line = command_line_overrides(args);
    let secret_from_env = admin.secret_override.is_some();
    let app = switchyard_server::router(gateway.clone())
        .merge(switchyard_admin::router(gateway.clone(), admin));

    let snapshot = gateway.scheduler().snapshot();
    let enabled = snapshot.iter().filter(|provider| provider.enabled);
    let info = StartInfo {
        listen: addr,
        tls,
        config_path: absolute(path),
        providers: enabled.clone().count(),
        credentials: enabled.map(|provider| provider.credentials.len()).sum(),
        models: gateway.scheduler().visible_models().len(),
        dashboard: Dashboard::of(&config, secret_from_env, &urls::base_url_of(addr, tls)),
        warnings: gateway.scheduler().warnings(),
        hints: banner::hints(&config),
    };
    out::stderr_line(&banner::start_banner(&info));
    // The one line scripts and tests wait for.
    out::stdout_line(&banner::listening_line(addr, tls));
    tracing::info!(%addr, tls, "listening");

    let served = bound.serve(app, shutdown_signal()).await;
    gateway.shutdown().await;
    match served {
        Ok(()) => {
            out::stderr_line("stopped");
            Ok(())
        }
        Err(error) => Err(CliError::failure(format!(
            "the server stopped with an error: {error}"
        ))),
    }
}

/// The settings `--host` / `--port` fix whatever the file says, as the
/// dotted paths the admin API names them by. A restart with the same
/// command line does not apply the file's value of these.
fn command_line_overrides(args: &ServeArgs) -> Vec<String> {
    let mut overridden = Vec::new();
    if args.host.is_some() {
        overridden.push("server.host".to_string());
    }
    if args.port.is_some() {
        overridden.push("server.port".to_string());
    }
    overridden
}

/// Explains why the gateway could not be started.
fn start_error(path: &Path, error: StartError) -> CliError {
    match error {
        StartError::Config(ConfigStoreError::Invalid(issues)) => {
            CliError::invalid_config(path, &issues)
        }
        StartError::Config(error) => {
            CliError::failure(format!("cannot load the configuration: {error}"))
        }
        StartError::Upstream(message) => CliError::failure(format!(
            "cannot set up connections to upstream providers: {message}"
        )),
    }
}

/// Explains why the listener could not be bound, naming the address and
/// the way out.
pub fn bind_error(host: &str, port: u16, error: &std::io::Error) -> CliError {
    let detail = error.to_string();
    // The certificate and the key are loaded by the same call, and a missing
    // or unreadable file has the same error kinds as a refused port.
    if detail.contains("TLS") {
        return CliError::failure(format!("cannot start the HTTPS listener: {detail}"));
    }
    let message = match error.kind() {
        ErrorKind::AddrInUse => format!(
            "cannot listen on {host}:{port}: port {port} is already in use. Stop the other \
             program, or choose another port with --port <PORT> (or server.port in the \
             configuration)"
        ),
        ErrorKind::PermissionDenied => format!(
            "cannot listen on {host}:{port}: not permitted to use port {port}. Choose another \
             with --port <PORT> (ports below 1024 usually need elevated rights)"
        ),
        ErrorKind::AddrNotAvailable => format!(
            "cannot listen on {host}:{port}: {host} is not an address of this machine. Check \
             --host (or server.host in the configuration)"
        ),
        // The server crate's own message already names the address.
        _ if detail.starts_with("cannot listen on") => detail,
        _ => format!("cannot listen on {host}:{port}: {detail}"),
    };
    CliError::failure(message)
}

/// Resolves when the process is asked to stop. A second request while the
/// server is draining ends the process at once.
async fn shutdown_signal() {
    stop_requested().await;
    out::stderr_line(
        "shutting down: waiting for requests in progress (press Ctrl-C again to stop now)",
    );
    tokio::spawn(async {
        stop_requested().await;
        out::stderr_line("stopped without waiting");
        std::process::exit(i32::from(EXIT_FAILURE));
    });
}

/// Ctrl-C, or SIGTERM (what `docker stop` and service managers send).
#[cfg(unix)]
async fn stop_requested() {
    use tokio::signal::unix::{SignalKind, signal};
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                () = interrupt => {}
                _ = terminate.recv() => {}
            }
        }
        Err(error) => {
            tracing::warn!(%error, "cannot listen for SIGTERM; only Ctrl-C stops the gateway");
            interrupt.await;
        }
    }
}

/// Ctrl-C, Ctrl-Break, or the console window being closed.
#[cfg(windows)]
async fn stop_requested() {
    use tokio::signal::windows::{ctrl_break, ctrl_close};
    // A source that cannot be set up never fires, instead of firing at
    // once.
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let brk = async {
        match ctrl_break() {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    let close = async {
        match ctrl_close() {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        () = interrupt => {}
        () = brk => {}
        () = close => {}
    }
}

#[cfg(not(any(unix, windows)))]
async fn stop_requested() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EXIT_USAGE;
    use switchyard_core::config::ConfigIssue;

    #[test]
    fn a_port_in_use_names_the_port_and_the_flag() {
        let error = bind_error(
            "127.0.0.1",
            8317,
            &std::io::Error::from(ErrorKind::AddrInUse),
        );
        assert_eq!(error.code, EXIT_FAILURE);
        assert!(
            error.message.contains("127.0.0.1:8317"),
            "{}",
            error.message
        );
        assert!(error.message.contains("port 8317 is already in use"));
        assert!(error.message.contains("--port"));
    }

    #[test]
    fn other_bind_failures_are_explained() {
        let denied = bind_error(
            "0.0.0.0",
            80,
            &std::io::Error::from(ErrorKind::PermissionDenied),
        );
        assert!(denied.message.contains("not permitted to use port 80"));
        assert!(denied.message.contains("--port"));
        let foreign = bind_error(
            "10.9.8.7",
            8317,
            &std::io::Error::from(ErrorKind::AddrNotAvailable),
        );
        assert!(foreign.message.contains("--host"));
        let other = bind_error("h", 1, &std::io::Error::other("socket exploded"));
        assert_eq!(other.message, "cannot listen on h:1: socket exploded");
        assert_eq!(other.code, EXIT_FAILURE);
        // A message that names the address already is not wrapped again.
        let named = bind_error(
            "h",
            1,
            &std::io::Error::other("cannot listen on h:1: no such host"),
        );
        assert_eq!(named.message, "cannot listen on h:1: no such host");
        // A certificate that cannot be read is not a port problem.
        let tls = bind_error(
            "h",
            443,
            &std::io::Error::new(
                ErrorKind::PermissionDenied,
                "TLS private key file key.pem: access denied",
            ),
        );
        assert_eq!(
            tls.message,
            "cannot start the HTTPS listener: TLS private key file key.pem: access denied"
        );
    }

    /// Regression (SU-5): the dashboard promised that a restart would apply
    /// a changed `server.port` that `--port` keeps overriding.
    #[test]
    fn host_and_port_flags_are_reported_as_command_line_overrides() {
        assert!(command_line_overrides(&ServeArgs::default()).is_empty());
        let port = ServeArgs {
            port: Some(18533),
            ..ServeArgs::default()
        };
        assert_eq!(command_line_overrides(&port), ["server.port"]);
        let both = ServeArgs {
            host: Some("0.0.0.0".into()),
            port: Some(0),
            ..ServeArgs::default()
        };
        assert_eq!(
            command_line_overrides(&both),
            ["server.host", "server.port"]
        );
    }

    #[test]
    fn start_errors_keep_their_issues() {
        let invalid = start_error(
            Path::new("switchyard.toml"),
            StartError::Config(ConfigStoreError::Invalid(vec![ConfigIssue {
                path: "server.port".into(),
                message: "must be between 1 and 65535".into(),
            }])),
        );
        assert_eq!(invalid.code, EXIT_USAGE);
        assert!(
            invalid
                .message
                .contains("\n  server.port: must be between 1 and 65535")
        );
        let upstream = start_error(Path::new("x"), StartError::Upstream("no roots".into()));
        assert_eq!(upstream.code, EXIT_FAILURE);
        assert!(upstream.message.contains("no roots"));
    }
}
