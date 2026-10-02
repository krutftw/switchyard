//! The one error type of the command line: an exit code and a sentence.

use std::fmt;
use std::path::Path;
use switchyard_core::config::ConfigIssue;

/// Everything went well.
pub const EXIT_OK: u8 = 0;
/// Something failed while running: a port in use, a file that cannot be
/// written, a server error.
pub const EXIT_FAILURE: u8 = 1;
/// The command line or the configuration is wrong.
pub const EXIT_USAGE: u8 = 2;

/// Why a command did not succeed.
///
/// The message is what the user reads on stderr: plain sentences, one issue
/// per line, never a `Debug` dump and never a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    /// The process exit code: [`EXIT_FAILURE`] or [`EXIT_USAGE`].
    pub code: u8,
    /// What to print after `error: `.
    pub message: String,
}

impl CliError {
    /// A failure at run time (exit code 1).
    pub fn failure(message: impl Into<String>) -> Self {
        CliError {
            code: EXIT_FAILURE,
            message: message.into(),
        }
    }

    /// A mistake in the command line or the configuration (exit code 2).
    pub fn usage(message: impl Into<String>) -> Self {
        CliError {
            code: EXIT_USAGE,
            message: message.into(),
        }
    }

    /// A configuration file that does not validate: every issue on its own
    /// line as `  path: message`.
    pub fn invalid_config(path: &Path, issues: &[ConfigIssue]) -> Self {
        CliError::usage(format!(
            "the configuration file {} is not valid:\n{}",
            path.display(),
            format_issues(issues)
        ))
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Renders configuration issues one per line, indented: `  path: message`.
pub fn format_issues(issues: &[ConfigIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("  {}: {}", issue.path, issue.message))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_are_listed_one_per_line() {
        let issues = vec![
            ConfigIssue {
                path: "server.port".into(),
                message: "must be between 1 and 65535".into(),
            },
            ConfigIssue {
                path: "providers[0].name".into(),
                message: "must not be empty".into(),
            },
        ];
        let error = CliError::invalid_config(Path::new("switchyard.toml"), &issues);
        assert_eq!(error.code, EXIT_USAGE);
        assert_eq!(
            error.to_string(),
            "the configuration file switchyard.toml is not valid:\n  \
             server.port: must be between 1 and 65535\n  \
             providers[0].name: must not be empty"
        );
    }

    #[test]
    fn codes() {
        assert_eq!(CliError::failure("x").code, 1);
        assert_eq!(CliError::usage("x").code, 2);
    }
}
