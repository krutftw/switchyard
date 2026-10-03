//! Command-line grammar and configuration-path lookup.

use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable naming the configuration file.
pub const CONFIG_ENV: &str = "SWITCHYARD_CONFIG";
/// Environment variable overriding `logging.level`.
pub const LOG_ENV: &str = "SWITCHYARD_LOG";
/// File used when neither `--config` nor [`CONFIG_ENV`] names one.
pub const DEFAULT_CONFIG_FILE: &str = "switchyard.toml";

const ENV_HELP: &str = "\
Environment variables:
  SWITCHYARD_CONFIG              Configuration file, used when --config is not given
  SWITCHYARD_ADMIN_SECRET        Admin secret; overrides admin.secret in the file
  SWITCHYARD_ADMIN_ALLOW_REMOTE  1 or true: accept admin requests from other machines
  SWITCHYARD_LOG                 Log level (trace, debug, info, warn, error) or filter
                                 directives such as \"info,switchyard_gateway=debug\";
                                 overrides logging.level

Run `switchyard` with no command to start the gateway. On the first run it
writes a starter configuration. An interactive terminal shows the admin secret\n\
and a client key once; redirected logs omit them. Read the private configuration\n\
file when starting as a service.";

/// The `switchyard` command line.
#[derive(Debug, Parser, PartialEq, Eq)]
#[command(
    name = "switchyard",
    // Not the file name of the executable (`switchyard.exe` on Windows).
    bin_name = "switchyard",
    version,
    about = "An LLM API gateway: OpenAI, Anthropic and Gemini compatible, with a built-in dashboard",
    after_help = ENV_HELP,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// What to do; `serve` when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Options of the implied `serve` command.
    #[command(flatten)]
    pub serve: ServeArgs,
}

impl Cli {
    /// The command to run: the one given, or `serve` with the top-level
    /// options.
    pub fn into_command(self) -> Command {
        self.command.unwrap_or(Command::Serve(self.serve))
    }
}

/// The subcommands.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Start the gateway (the default command)
    Serve(ServeArgs),
    /// Write a starter configuration with a fresh admin secret and client key
    Init(InitArgs),
    /// Validate the configuration and print a summary with warnings
    Check(CheckArgs),
    /// Convert the API-key sections of a CLIProxyAPI config.yaml into a Switchyard configuration
    #[command(name = "import-cliproxy")]
    ImportCliproxy(ImportArgs),
    /// Print the version
    Version,
}

/// Options of `serve`.
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct ServeArgs {
    /// Configuration file [default: $SWITCHYARD_CONFIG, else ./switchyard.toml]
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Address to listen on, overriding server.host
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// Port to listen on, overriding server.port (0 picks a free port)
    #[arg(short, long, value_name = "PORT")]
    pub port: Option<u16>,
}

/// Options of `init`.
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct InitArgs {
    /// File to write [default: $SWITCHYARD_CONFIG, else ./switchyard.toml]
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Replace the file if it already exists
    #[arg(long)]
    pub force: bool,
}

/// Options of `check`.
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckArgs {
    /// File to check [default: $SWITCHYARD_CONFIG, else ./switchyard.toml]
    #[arg(short, long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// Options of `import-cliproxy`.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct ImportArgs {
    /// The CLIProxyAPI config.yaml to read (flat or nested "v8" layout)
    #[arg(value_name = "CONFIG_YAML")]
    pub input: PathBuf,

    /// File to write
    #[arg(short, long, value_name = "PATH", default_value = DEFAULT_CONFIG_FILE)]
    pub output: PathBuf,

    /// Replace the output file if it already exists
    #[arg(long)]
    pub force: bool,
}

/// Picks the configuration file: the `--config` value, else the value of
/// `SWITCHYARD_CONFIG` (when set and not blank), else `./switchyard.toml`.
pub fn resolve_config_path(flag: Option<&Path>, env: Option<OsString>) -> PathBuf {
    if let Some(path) = flag {
        return path.to_path_buf();
    }
    match env {
        Some(value) if !value.to_string_lossy().trim().is_empty() => PathBuf::from(value),
        _ => PathBuf::from(DEFAULT_CONFIG_FILE),
    }
}

/// [`resolve_config_path`] with the process environment.
pub fn config_path(flag: Option<&Path>) -> PathBuf {
    resolve_config_path(flag, std::env::var_os(CONFIG_ENV))
}

/// The version line printed by `switchyard version` and `--version`.
pub fn version_line() -> String {
    format!("switchyard {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("switchyard").chain(args.iter().copied()))
    }

    #[test]
    fn the_grammar_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_arguments_means_serve() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.command, None);
        assert_eq!(cli.into_command(), Command::Serve(ServeArgs::default()));
    }

    #[test]
    fn serve_options_work_with_and_without_the_word() {
        let expected = Command::Serve(ServeArgs {
            config: Some(PathBuf::from("my dir/gateway.toml")),
            host: Some("0.0.0.0".into()),
            port: Some(0),
        });
        let implied = parse(&[
            "--config",
            "my dir/gateway.toml",
            "--host",
            "0.0.0.0",
            "--port",
            "0",
        ])
        .unwrap();
        assert_eq!(implied.into_command(), expected);
        let explicit = parse(&[
            "serve",
            "--config",
            "my dir/gateway.toml",
            "--host",
            "0.0.0.0",
            "--port",
            "0",
        ])
        .unwrap();
        assert_eq!(explicit.into_command(), expected);
    }

    #[test]
    fn init_check_and_version() {
        assert_eq!(
            parse(&["init", "--config", "x.toml", "--force"])
                .unwrap()
                .into_command(),
            Command::Init(InitArgs {
                config: Some(PathBuf::from("x.toml")),
                force: true,
            })
        );
        assert_eq!(
            parse(&["init"]).unwrap().into_command(),
            Command::Init(InitArgs::default())
        );
        assert_eq!(
            parse(&["check", "--config", "x.toml"])
                .unwrap()
                .into_command(),
            Command::Check(CheckArgs {
                config: Some(PathBuf::from("x.toml")),
            })
        );
        assert_eq!(
            parse(&["version"]).unwrap().into_command(),
            Command::Version
        );
    }

    #[test]
    fn import_takes_an_input_and_an_optional_output() {
        assert_eq!(
            parse(&["import-cliproxy", "config.yaml"])
                .unwrap()
                .into_command(),
            Command::ImportCliproxy(ImportArgs {
                input: PathBuf::from("config.yaml"),
                output: PathBuf::from("switchyard.toml"),
                force: false,
            })
        );
        assert_eq!(
            parse(&[
                "import-cliproxy",
                "old/config.yaml",
                "-o",
                "new.toml",
                "--force"
            ])
            .unwrap()
            .into_command(),
            Command::ImportCliproxy(ImportArgs {
                input: PathBuf::from("old/config.yaml"),
                output: PathBuf::from("new.toml"),
                force: true,
            })
        );
        let missing = parse(&["import-cliproxy"]).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(missing.exit_code(), 2);
    }

    #[test]
    fn mistakes_are_usage_errors() {
        for args in [
            &["--port", "70000"][..],
            &["--port", "abc"],
            &["--bogus"],
            &["frobnicate"],
            &["check", "--port", "1"],
        ] {
            let error = parse(args).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn version_flags_and_help_exit_zero() {
        for flag in ["-V", "--version"] {
            let error = parse(&[flag]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::DisplayVersion);
            assert_eq!(error.exit_code(), 0);
            assert_eq!(error.to_string().trim(), version_line());
        }
        let help = parse(&["--help"]).unwrap_err();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert_eq!(help.exit_code(), 0);
    }

    #[test]
    fn help_names_every_command_flag_and_environment_variable() {
        let help = Cli::command().render_long_help().to_string();
        for needle in [
            "serve",
            "init",
            "check",
            "import-cliproxy",
            "version",
            "--config",
            "--host",
            "--port",
            "SWITCHYARD_CONFIG",
            "SWITCHYARD_ADMIN_SECRET",
            "SWITCHYARD_ADMIN_ALLOW_REMOTE",
            "SWITCHYARD_LOG",
        ] {
            assert!(help.contains(needle), "help lacks {needle}:\n{help}");
        }
    }

    #[test]
    fn every_command_and_flag_has_help() {
        fn walk(command: &clap::Command) {
            for argument in command.get_arguments() {
                assert!(
                    argument.get_help().is_some(),
                    "`{}` of `{}` has no help",
                    argument.get_id(),
                    command.get_name()
                );
            }
            for sub in command.get_subcommands() {
                assert!(
                    sub.get_about().is_some(),
                    "command `{}` has no description",
                    sub.get_name()
                );
                walk(sub);
            }
        }
        let mut command = Cli::command();
        command.build();
        walk(&command);
    }

    #[test]
    fn config_path_precedence() {
        // The flag wins.
        assert_eq!(
            resolve_config_path(Some(Path::new("flag.toml")), Some("env.toml".into())),
            PathBuf::from("flag.toml")
        );
        // Then the environment.
        assert_eq!(
            resolve_config_path(None, Some("with space/env.toml".into())),
            PathBuf::from("with space/env.toml")
        );
        // A blank variable counts as unset.
        assert_eq!(
            resolve_config_path(None, Some("  ".into())),
            PathBuf::from("switchyard.toml")
        );
        assert_eq!(
            resolve_config_path(None, None),
            PathBuf::from("switchyard.toml")
        );
    }
}
