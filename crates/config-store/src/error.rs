//! The error type of the configuration store.

use std::fmt;
use std::path::PathBuf;
use switchyard_core::config::ConfigIssue;

/// Why a configuration could not be loaded, edited or persisted.
#[derive(Debug)]
pub enum ConfigStoreError {
    /// The configuration file could not be read or written.
    Io {
        /// The file (or directory) the operation was about.
        path: PathBuf,
        source: std::io::Error,
    },
    /// The configuration is not valid TOML, does not match the schema, or
    /// breaks a semantic rule. Every problem found is listed.
    Invalid(Vec<ConfigIssue>),
    /// A typed edit was refused by its own closure, or its result cannot be
    /// written as TOML.
    Edit(String),
    /// An edit ([`ConfigStore::update`](crate::ConfigStore::update)) was
    /// refused because the file on disk holds content that is not the live
    /// configuration and is not valid — a manual edit in progress, or one
    /// that was rejected. Writing the edit would overwrite that content from
    /// the last valid configuration. The issues are those of the file on
    /// disk, not of the edit; nothing was written.
    DiskInvalid(Vec<ConfigIssue>),
}

impl ConfigStoreError {
    /// The validation issues behind this error: of the configuration that
    /// was refused ([`ConfigStoreError::Invalid`]) or of the file on disk
    /// ([`ConfigStoreError::DiskInvalid`]); empty otherwise.
    pub fn issues(&self) -> &[ConfigIssue] {
        match self {
            ConfigStoreError::Invalid(issues) | ConfigStoreError::DiskInvalid(issues) => issues,
            _ => &[],
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        ConfigStoreError::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for ConfigStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigStoreError::Io { path, source } => {
                write!(f, "cannot access {}: {source}", path.display())
            }
            ConfigStoreError::Invalid(issues) => {
                f.write_str("invalid configuration")?;
                for (i, issue) in issues.iter().enumerate() {
                    f.write_str(if i == 0 { ": " } else { "; " })?;
                    write!(f, "{issue}")?;
                }
                Ok(())
            }
            ConfigStoreError::Edit(message) => f.write_str(message),
            ConfigStoreError::DiskInvalid(issues) => {
                f.write_str(
                    "the configuration file on disk is not valid and was not overwritten; \
                     fix or restore the file, or replace it through the raw editor",
                )?;
                for (i, issue) in issues.iter().enumerate() {
                    f.write_str(if i == 0 { ": " } else { "; " })?;
                    write!(f, "{issue}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigStoreError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_lists_every_issue() {
        let err = ConfigStoreError::Invalid(vec![
            ConfigIssue {
                path: "server.port".into(),
                message: "must be between 1 and 65535".into(),
            },
            ConfigIssue {
                path: "providers[0].name".into(),
                message: "must not be empty".into(),
            },
        ]);
        assert_eq!(
            err.to_string(),
            "invalid configuration: server.port: must be between 1 and 65535; \
             providers[0].name: must not be empty"
        );
        assert_eq!(err.issues().len(), 2);
    }

    #[test]
    fn disk_invalid_says_what_to_do_and_lists_the_files_issues() {
        let err = ConfigStoreError::DiskInvalid(vec![ConfigIssue {
            path: "line 2, column 8".into(),
            message: "invalid TOML".into(),
        }]);
        assert_eq!(
            err.to_string(),
            "the configuration file on disk is not valid and was not overwritten; fix or \
             restore the file, or replace it through the raw editor: line 2, column 8: \
             invalid TOML"
        );
        assert_eq!(err.issues().len(), 1);
    }

    #[test]
    fn io_and_edit_have_no_issues() {
        let io = ConfigStoreError::io(
            "x/switchyard.toml",
            std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        );
        assert!(io.issues().is_empty());
        assert!(io.to_string().contains("switchyard.toml"));
        assert!(std::error::Error::source(&io).is_some());
        let edit = ConfigStoreError::Edit("no such provider".into());
        assert_eq!(edit.to_string(), "no such provider");
        assert!(edit.issues().is_empty());
    }
}
