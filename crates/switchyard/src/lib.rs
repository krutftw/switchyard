//! The `switchyard` command line. See `docs/DESIGN.md` section 13.
//!
//! ```text
//! switchyard [serve] [--config <path>] [--host <h>] [--port <p>]
//! switchyard init [--config <path>] [--force]
//! switchyard check [--config <path>]
//! switchyard import-cliproxy <config.yaml> [-o <path>] [--force]
//! switchyard version
//! ```
//!
//! The binary is a thin `main` around [`run`]; the commands live in modules
//! that can be tested without a process:
//!
//! * [`cli`] — the grammar and the configuration-path lookup (`--config`,
//!   then `SWITCHYARD_CONFIG`, then `./switchyard.toml`);
//! * [`starter`] — the configuration `init` and the first `serve` write;
//! * [`check`] — validation, summary and warnings;
//! * [`import`] (with [`yaml`]) — the CLIProxyAPI importer;
//! * [`logging`] — the log subscriber: stderr, the dashboard's capture
//!   layer, a level that follows the configuration;
//! * [`serve`] (with [`banner`]) — starting, announcing and stopping the
//!   gateway.
//!
//! Exit codes: `0` success, `1` a failure at run time, `2` a mistake in the
//! command line or the configuration. Errors are sentences on stderr.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod banner;
pub mod check;
pub mod cli;
pub mod error;
pub mod import;
pub mod logging;
pub mod out;
pub mod serve;
pub mod starter;
pub mod urls;
pub mod yaml;

use clap::Parser;
use cli::{Cli, Command};
use error::{CliError, EXIT_OK, EXIT_USAGE};
use std::process::ExitCode;

/// Parses the process's arguments, runs the command and returns the exit
/// code.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // Help and version requests arrive here too, with code 0.
            let _ = error.print();
            let code = u8::try_from(error.exit_code()).unwrap_or(EXIT_USAGE);
            return ExitCode::from(code);
        }
    };
    match dispatch(cli.into_command()) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            out::stderr_line(&format!("error: {error}"));
            ExitCode::from(error.code)
        }
    }
}

/// Runs one command.
pub fn dispatch(command: Command) -> Result<(), CliError> {
    match command {
        Command::Serve(args) => serve::run(&args),
        Command::Init(args) => {
            let path = cli::config_path(args.config.as_deref());
            let created = starter::create(&path, args.force)?;
            out::stdout_line(&created.announcement(Some(&starter::starter_dashboard_url())));
            out::stdout_line(&format!(
                "Start the gateway with: switchyard --config \"{}\"",
                created.path.display()
            ));
            Ok(())
        }
        Command::Check(args) => {
            let path = cli::config_path(args.config.as_deref());
            let report = check::run(&path, &check::process_env)?;
            out::stdout_text(&report);
            Ok(())
        }
        Command::ImportCliproxy(args) => {
            let output = import::run(&args, &check::process_env)?;
            out::stderr_text(&output.stderr);
            out::stdout_text(&output.stdout);
            Ok(())
        }
        Command::Version => {
            out::stdout_line(&cli::version_line());
            Ok(())
        }
    }
}
