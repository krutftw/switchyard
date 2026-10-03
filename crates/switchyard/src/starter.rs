//! The starter configuration written by `switchyard init` and by the first
//! `switchyard serve`.

use crate::error::CliError;
use crate::urls;
use rand::Rng;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use switchyard_core::config::DEFAULT_PORT;

/// Host the starter configuration listens on.
pub const STARTER_HOST: &str = "127.0.0.1";
/// Name of the client key in the starter configuration.
pub const STARTER_KEY_NAME: &str = "default";
/// Prefix of generated client keys.
pub const CLIENT_KEY_PREFIX: &str = "sy-";

const ADMIN_SECRET_CHARS: usize = 32;
const CLIENT_KEY_CHARS: usize = 40;

/// Characters secrets are made of: letters and digits only, so a secret is
/// safe in a URL, a shell command and a TOML string, and never starts with
/// a dash. 62 symbols give just under six bits each.
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// The two secrets a starter configuration contains.
#[derive(Clone, PartialEq, Eq)]
pub struct StarterSecrets {
    /// Secret of the dashboard and the admin API.
    pub admin_secret: String,
    /// The client key named `default`.
    pub client_key: String,
}

impl std::fmt::Debug for StarterSecrets {
    /// Never prints the secrets: the type may end up in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StarterSecrets").finish_non_exhaustive()
    }
}

impl StarterSecrets {
    /// Generates fresh secrets from the operating system's entropy (through
    /// the thread-local ChaCha generator).
    pub fn generate() -> Self {
        StarterSecrets {
            admin_secret: random_token(ADMIN_SECRET_CHARS),
            client_key: format!("{CLIENT_KEY_PREFIX}{}", random_token(CLIENT_KEY_CHARS)),
        }
    }
}

fn random_token(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect()
}

/// The `--host` / `--port` flags of the run that writes the starter
/// configuration (the first `serve`). They override the file for that run,
/// so the file says so instead of naming an address that is not in use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `--host`, when given.
    pub host: Option<String>,
    /// `--port`, when given (`0`: a free port chosen at start-up).
    pub port: Option<u16>,
}

impl Flags {
    /// The lines the `[server]` section opens with: which flags this run
    /// used and that the values below apply without them. Empty without
    /// flags.
    fn server_note(&self) -> String {
        let host = self.shown_host();
        let port = self.port.map(|port| match port {
            0 => "--port 0 (a free port chosen at start-up)".to_string(),
            port => format!("--port {port}"),
        });
        let (used, without, values) = match (host, port) {
            (None, None) => return String::new(),
            (Some(host), None) => (format!("--host {host}"), "--host", "the host below applies"),
            (None, Some(port)) => (port, "--port", "the port below applies"),
            (Some(host), Some(port)) => (
                format!("--host {host} {port}"),
                "them",
                "the host and port below apply",
            ),
        };
        format!(
            "# This first run used {used}; without {without} {values}.\n\
             # A flag on the command line always overrides the value in this file.\n"
        )
    }

    /// `--host` as it may be shown in a comment: a control character (a
    /// line break) would end the comment.
    fn shown_host(&self) -> Option<String> {
        self.host
            .as_deref()
            .map(|host| host.chars().filter(|c| !c.is_control()).collect())
    }

    /// The line(s) the `[admin]` section opens with: what the secret is for
    /// and where the dashboard of this run is.
    fn admin_note(&self) -> String {
        let host = self.shown_host();
        let host = host.as_deref().unwrap_or(STARTER_HOST);
        match self.port.unwrap_or(DEFAULT_PORT) {
            0 => "# Secret for the dashboard (at /admin/ on the address the gateway prints at\n\
                  # start-up) and the admin API.\n"
                .to_string(),
            port => {
                let url = urls::dashboard_url(&urls::base_url(host, port, false));
                if *self == Flags::default() {
                    format!("# Secret for the dashboard ({url}) and the admin API.\n")
                } else {
                    format!(
                        "# Secret for the dashboard and the admin API. Started as on the first\n\
                         # run, the dashboard is at {url}\n"
                    )
                }
            }
        }
    }
}

/// The text of a starter configuration: loopback listener, the given admin
/// secret and client key, the mock provider, and commented-out examples of
/// the real provider kinds.
pub fn starter_config(secrets: &StarterSecrets) -> String {
    starter_config_for(secrets, &Flags::default())
}

/// [`starter_config`] written by a run with these command-line flags.
pub fn starter_config_for(secrets: &StarterSecrets, flags: &Flags) -> String {
    let admin_secret = &secrets.admin_secret;
    let client_key = &secrets.client_key;
    let server_note = flags.server_note();
    let admin_note = flags.admin_note();
    format!(
        r#"# Switchyard configuration, written by `switchyard init`.
#
# The gateway reloads this file when it changes; the dashboard edits it in
# place and keeps your comments. Only a few settings are listed here: every
# setting, with its default and an explanation, is in switchyard.example.toml.
#
# Secrets can be written literally or as a reference to an environment
# variable: "env:OPENAI_API_KEY" or "${{OPENAI_API_KEY}}".

[server]
{server_note}host = "{STARTER_HOST}"        # "0.0.0.0" to accept connections from other machines
port = {DEFAULT_PORT}

[admin]
{admin_note}# It was generated for this file. SWITCHYARD_ADMIN_SECRET in the environment
# overrides it; with no secret at all, both are switched off.
secret = "{admin_secret}"

# Keys your clients present: Authorization: Bearer …, x-api-key,
# x-goog-api-key or ?key=. Add more in the dashboard, or as further blocks
# like this one. A key can be limited to some models and to a request rate.
[[auth.keys]]
name = "{STARTER_KEY_NAME}"
key = "{client_key}"

# ---------------------------------------------------------------------------
# Providers. `api_keys` lists one credential per key; requests rotate across
# them and fail over when one is rate limited.

# The built-in mock provider needs no key and no network. Its models
# (mock-echo, mock-lorem, mock-think, mock-tools, …) let you try clients and
# the dashboard right away. Delete it once a real provider works.
[[providers]]
name = "mock"
kind = "mock"

# Uncomment and fill in what you use. More options (prefix, priority, proxy,
# headers, per-credential weights, model aliases, Vertex service accounts,
# virtual models, payload rules, prices) are shown in switchyard.example.toml.

# [[providers]]
# name = "openai"
# kind = "openai"                       # api.openai.com: Responses and Chat Completions
# api_keys = ["env:OPENAI_API_KEY"]

# [[providers]]
# name = "anthropic"
# kind = "anthropic"
# api_keys = ["env:ANTHROPIC_API_KEY"]

# [[providers]]
# name = "gemini"
# kind = "gemini"
# api_keys = ["env:GEMINI_API_KEY"]

# Any OpenAI-compatible server (OpenRouter, Groq, DeepSeek, vLLM, …):
# [[providers]]
# name = "openrouter"
# kind = "openai-compat"
# base_url = "https://openrouter.ai/api/v1"
# api_keys = ["env:OPENROUTER_API_KEY"]
# prefix = "or"                          # models become "or/<model>"

# A local server needs no key:
# [[providers]]
# name = "ollama"
# kind = "openai-compat"
# base_url = "http://127.0.0.1:11434/v1"
"#
    )
}

/// A starter configuration that was just written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    /// The file, as an absolute path when that can be determined.
    pub path: PathBuf,
    /// The secrets inside it.
    pub secrets: StarterSecrets,
}

impl Created {
    /// What `init` and the first `serve` tell the user: where the file is,
    /// the admin secret, the client key and where the dashboard will be.
    ///
    /// `dashboard` is the dashboard URL; `None` when the port is not known
    /// yet (`serve --port 0`), in which case the start banner names it.
    pub fn announcement(&self, dashboard: Option<&str>) -> String {
        self.announcement_with_secrets(dashboard, true)
    }

    /// Server startup logs omit credentials when stderr is redirected.
    pub fn announcement_with_secrets(&self, dashboard: Option<&str>, show_secrets: bool) -> String {
        let mut text = format!("wrote a starter configuration to {}\n", self.path.display(),);
        if show_secrets {
            text.push_str(&format!(
                "\
             \x20 admin secret  {}\n\
             \x20 client key    {}  (name: {STARTER_KEY_NAME})\n",
                self.secrets.admin_secret, self.secrets.client_key,
            ));
        }
        match dashboard {
            Some(url) => text.push_str(&format!("  dashboard     {url}\n")),
            None => text.push_str("  dashboard     see the address below, under /admin/\n"),
        }
        text.push_str(if show_secrets {
            "Both secrets are stored in that file and are not shown again. "
        } else {
            "Read the admin secret and client key from that file; credentials are omitted from startup logs. "
        });
        text.push_str("The mock provider is enabled, so clients can connect right away.");
        text
    }
}

/// The dashboard URL of an untouched starter configuration.
pub fn starter_dashboard_url() -> String {
    urls::dashboard_url(&urls::base_url(STARTER_HOST, DEFAULT_PORT, false))
}

/// Writes a starter configuration with fresh secrets to `path`.
///
/// An existing file is left alone unless `force` is set, in which case it is
/// replaced. Missing parent directories are created.
pub fn create(path: &Path, force: bool) -> Result<Created, CliError> {
    create_for(path, force, &Flags::default())
}

/// [`create`] for a run with these command-line flags (see [`Flags`]).
pub fn create_for(path: &Path, force: bool, flags: &Flags) -> Result<Created, CliError> {
    let secrets = StarterSecrets::generate();
    let text = starter_config_for(&secrets, flags);
    write_new_file(path, &text, force)?;
    Ok(Created {
        path: absolute(path),
        secrets,
    })
}

/// Creates `path` with `text` through the configuration store's writer
/// (owner-only permissions on Unix, never a partial file). With `force` an
/// existing file is removed first.
pub fn write_new_file(path: &Path, text: &str, force: bool) -> Result<(), CliError> {
    if path.is_dir() {
        return Err(CliError::usage(format!(
            "{} is a directory; name a file",
            path.display()
        )));
    }
    if force {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CliError::failure(format!(
                    "cannot replace {}: {error}",
                    path.display()
                )));
            }
        }
    }
    match switchyard_config_store::write_new(path, text) {
        Ok(()) => Ok(()),
        // Creating a parent directory can fail with the same kind (a file
        // is in the way); only the target itself existing is the refusal.
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.exists() => {
            Err(CliError::usage(format!(
                "{} already exists; pass --force to replace it",
                path.display()
            )))
        }
        Err(error) => Err(CliError::failure(format!(
            "cannot write {}: {error}",
            path.display()
        ))),
    }
}

/// `path` made absolute against the working directory, for messages. Falls
/// back to the path as given.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EXIT_FAILURE, EXIT_USAGE};
    use pretty_assertions::assert_eq;
    use switchyard_config_store::validate_text;
    use switchyard_core::config::ProviderKind;

    fn fixed() -> StarterSecrets {
        StarterSecrets {
            admin_secret: "A".repeat(32),
            client_key: format!("sy-{}", "b".repeat(40)),
        }
    }

    fn url_safe(text: &str) -> bool {
        text.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    #[test]
    fn generated_secrets_have_the_documented_shape() {
        let a = StarterSecrets::generate();
        let b = StarterSecrets::generate();
        assert_eq!(a.admin_secret.chars().count(), 32);
        assert!(url_safe(&a.admin_secret));
        let tail = a.client_key.strip_prefix("sy-").expect("sy- prefix");
        assert_eq!(tail.chars().count(), 40);
        assert!(url_safe(tail));
        assert_ne!(a.admin_secret, b.admin_secret);
        assert_ne!(a.client_key, b.client_key);
        assert_ne!(a.admin_secret.as_str(), &tail[..32]);
    }

    #[test]
    fn debug_hides_the_secrets() {
        let secrets = fixed();
        let shown = format!("{secrets:?}");
        assert!(!shown.contains(&secrets.admin_secret));
        assert!(!shown.contains(&secrets.client_key));
    }

    #[test]
    fn starter_config_parses_and_validates() {
        let secrets = StarterSecrets::generate();
        let config = validate_text(&starter_config(&secrets)).expect("starter config is valid");
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8317);
        assert_eq!(config.admin.secret, secrets.admin_secret);
        assert!(config.admin.enabled);
        assert!(!config.admin.allow_remote);
        assert!(config.auth.required);
        assert_eq!(config.auth.keys.len(), 1);
        assert_eq!(config.auth.keys[0].name, "default");
        assert_eq!(config.auth.keys[0].key, secrets.client_key);
        assert!(config.auth.keys[0].enabled);
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "mock");
        assert_eq!(config.providers[0].kind, ProviderKind::Mock);
        assert!(config.providers[0].enabled);
    }

    /// The commented-out examples must be valid once uncommented, or the
    /// file would teach a broken configuration.
    #[test]
    fn commented_examples_are_valid_too() {
        let text = starter_config(&fixed());
        let mut uncommented = String::new();
        for line in text.lines() {
            let candidate = line.strip_prefix("# ").unwrap_or("");
            let looks_like_toml = candidate.starts_with("[[")
                || candidate.split_once(" = ").is_some_and(|(key, _)| {
                    !key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                });
            uncommented.push_str(if looks_like_toml { candidate } else { line });
            uncommented.push('\n');
        }
        let config = validate_text(&uncommented)
            .unwrap_or_else(|issues| panic!("{issues:?}\n---\n{uncommented}"));
        let kinds: Vec<&str> = config.providers.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "mock",
                "openai",
                "anthropic",
                "gemini",
                "openai-compat",
                "openai-compat"
            ]
        );
    }

    /// Regression (A2-2): a first run with `--port` wrote a file whose
    /// comment named the dashboard at the default port, which that run did
    /// not use, and nothing said the flag overrode the file.
    #[test]
    fn a_first_run_with_flags_names_the_address_in_use() {
        let plain = starter_config(&fixed());
        assert!(plain.contains("# Secret for the dashboard (http://127.0.0.1:8317/admin/) and"));
        assert!(!plain.contains("first run"));

        let port = Flags {
            host: None,
            port: Some(18534),
        };
        let text = starter_config_for(&fixed(), &port);
        assert!(
            text.contains(
                "[server]\n# This first run used --port 18534; without --port the port below \
                 applies.\n# A flag on the command line always overrides the value in this \
                 file.\nhost = \"127.0.0.1\""
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "[admin]\n# Secret for the dashboard and the admin API. Started as on the first\n\
                 # run, the dashboard is at http://127.0.0.1:18534/admin/\n"
            ),
            "{text}"
        );

        let both = Flags {
            host: Some("0.0.0.0".into()),
            port: Some(9000),
        };
        let text = starter_config_for(&fixed(), &both);
        assert!(
            text.contains(
                "# This first run used --host 0.0.0.0 --port 9000; without them the host and port \
                 below apply."
            ),
            "{text}"
        );
        // A wildcard is reached on loopback.
        assert!(
            text.contains("dashboard is at http://127.0.0.1:9000/admin/\n"),
            "{text}"
        );

        // Nothing a flag holds can break out of the comments.
        let host = Flags {
            host: Some("gw.local\nport = 1".into()),
            port: None,
        };
        let text = starter_config_for(&fixed(), &host);
        assert!(
            text.contains(
                "# This first run used --host gw.localport = 1; without --host the host below \
                 applies."
            ),
            "{text}"
        );
        assert!(!text.contains("\nport = 1"), "{text}");

        let any_port = Flags {
            host: None,
            port: Some(0),
        };
        let text = starter_config_for(&fixed(), &any_port);
        assert!(
            text.contains("--port 0 (a free port chosen at start-up)"),
            "{text}"
        );
        assert!(
            text.contains("(at /admin/ on the address the gateway prints at\n# start-up)"),
            "{text}"
        );

        // Whatever the flags, the file itself is the same configuration.
        for flags in [port, both, host, any_port] {
            let config = validate_text(&starter_config_for(&fixed(), &flags)).unwrap();
            assert_eq!(config, validate_text(&plain).unwrap(), "{flags:?}");
        }
    }

    #[test]
    fn starter_config_points_to_the_full_example() {
        let text = starter_config(&fixed());
        assert!(text.contains("switchyard.example.toml"));
        assert!(text.contains("${OPENAI_API_KEY}"));
    }

    #[test]
    fn create_writes_once_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("with space").join("switchyard.toml");
        let created = create(&path, false).unwrap();
        assert!(created.path.is_absolute());
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains(&created.secrets.admin_secret));
        assert!(first.contains(&created.secrets.client_key));
        validate_text(&first).unwrap();

        let refused = create(&path, false).unwrap_err();
        assert_eq!(refused.code, EXIT_USAGE);
        assert!(refused.message.contains("--force"), "{}", refused.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);

        let replaced = create(&path, true).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_ne!(first, second);
        assert!(second.contains(&replaced.secrets.admin_secret));
    }

    #[test]
    fn a_directory_is_not_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        for force in [false, true] {
            let error = create(dir.path(), force).unwrap_err();
            assert_eq!(error.code, EXIT_USAGE);
            assert!(error.message.contains("directory"), "{}", error.message);
        }
        assert!(dir.path().is_dir());
    }

    #[test]
    fn unwritable_location_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain-file");
        std::fs::write(&file, "x").unwrap();
        // A file where a directory is needed.
        let error = create(&file.join("switchyard.toml"), false).unwrap_err();
        assert_eq!(error.code, EXIT_FAILURE);
        assert!(error.message.starts_with("cannot write"));
    }

    #[test]
    fn announcement_lists_everything_once() {
        let created = Created {
            path: PathBuf::from("/tmp/switchyard.toml"),
            secrets: fixed(),
        };
        let text = created.announcement(Some("http://127.0.0.1:8317/admin/"));
        assert!(text.contains("switchyard.toml"));
        assert_eq!(text.matches(&created.secrets.admin_secret).count(), 1);
        assert_eq!(text.matches(&created.secrets.client_key).count(), 1);
        assert!(text.contains("http://127.0.0.1:8317/admin/"));
        let pending = created.announcement(None);
        assert!(pending.contains("/admin/"));
        assert_eq!(starter_dashboard_url(), "http://127.0.0.1:8317/admin/");
        let logged = created.announcement_with_secrets(None, false);
        assert!(!logged.contains(&created.secrets.admin_secret));
        assert!(!logged.contains(&created.secrets.client_key));
        assert!(logged.contains("switchyard.toml"));
    }
}
