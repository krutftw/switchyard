//! `switchyard check`: validate a configuration file and point out what
//! will not work as written.

use crate::error::CliError;
use crate::starter::absolute;
use crate::urls::is_loopback_host;
use std::io::ErrorKind;
use std::path::Path;
use switchyard_config_store::validate_text;
use switchyard_core::Config;

/// Environment variable that overrides `admin.secret`.
pub const ADMIN_SECRET_ENV: &str = "SWITCHYARD_ADMIN_SECRET";
/// Environment variable that overrides `admin.allow_remote`.
pub const ADMIN_ALLOW_REMOTE_ENV: &str = "SWITCHYARD_ADMIN_ALLOW_REMOTE";

/// Looks an environment variable up. A parameter so that tests do not
/// depend on (or change) the process environment.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment as an [`EnvLookup`].
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// What `check` found in a valid configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Lines describing what is configured.
    pub summary: Vec<String>,
    /// Things that validate but will not work, or are risky.
    pub warnings: Vec<String>,
}

/// The variable a secret value refers to (`env:NAME` or `${NAME}`), if it is
/// a reference. Mirrors `switchyard_core::config::resolve_secret`.
pub fn secret_reference(value: &str) -> Option<&str> {
    let value = value.trim();
    match value.strip_prefix("env:") {
        Some(name) => Some(name.trim()),
        None => value
            .strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
            .map(str::trim),
    }
}

fn is_set(env: EnvLookup<'_>, name: &str) -> bool {
    env(name).is_some_and(|value| !value.trim().is_empty())
}

fn is_truthy(env: EnvLookup<'_>, name: &str) -> bool {
    env(name).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Summarises a valid configuration and collects warnings.
///
/// `config_dir` is the directory relative file names in the configuration
/// resolve against; when given, files the configuration names are checked
/// for existence.
pub fn inspect(config: &Config, env: EnvLookup<'_>, config_dir: Option<&Path>) -> CheckReport {
    let mut report = CheckReport::default();
    let unset = |path: String, value: &str, warnings: &mut Vec<String>| {
        if let Some(name) = secret_reference(value)
            && !is_set(env, name)
        {
            if name.is_empty() {
                warnings.push(format!("{path} is an empty environment reference"));
            } else {
                warnings.push(format!(
                    "{path} refers to the environment variable {name}, which is not set"
                ));
            }
        }
    };

    // Listener.
    let scheme = if config.server.tls.is_some() {
        "https"
    } else {
        "http"
    };
    report.summary.push(format!(
        "listen       {}:{} ({scheme})",
        config.server.host.trim(),
        config.server.port
    ));
    if let (Some(tls), Some(dir)) = (&config.server.tls, config_dir) {
        for (field, file) in [("cert", &tls.cert), ("key", &tls.key)] {
            if !dir.join(file.trim()).is_file() {
                report
                    .warnings
                    .push(format!("server.tls.{field}: the file does not exist"));
            }
        }
    }

    // Admin API.
    let secret_from_env = is_set(env, ADMIN_SECRET_ENV);
    let allow_remote = config.admin.allow_remote || is_truthy(env, ADMIN_ALLOW_REMOTE_ENV);
    let local_only = is_loopback_host(&config.server.host);
    if !config.admin.enabled {
        report.summary.push("admin        disabled".to_string());
    } else {
        let has_secret = secret_from_env || !config.admin.secret.trim().is_empty();
        if !secret_from_env {
            unset(
                "admin.secret".to_string(),
                &config.admin.secret,
                &mut report.warnings,
            );
        }
        report.summary.push(format!(
            "admin        {}, {}{}",
            match (has_secret, secret_from_env) {
                (_, true) => "secret from SWITCHYARD_ADMIN_SECRET",
                (true, false) => "secret set",
                (false, false) => "no secret",
            },
            if allow_remote {
                "remote connections allowed"
            } else {
                "local connections only"
            },
            if config.admin.ui {
                ""
            } else {
                ", dashboard off"
            }
        ));
        if !has_secret {
            report.warnings.push(
                "no admin secret is set (admin.secret or SWITCHYARD_ADMIN_SECRET): \
                 the dashboard and the admin API are switched off"
                    .to_string(),
            );
        }
        if allow_remote && !local_only {
            report.warnings.push(format!(
                "server.host is {} and admin.allow_remote is true: the admin API and the \
                 dashboard accept connections from other machines",
                config.server.host.trim()
            ));
        }
    }

    // Client keys.
    let keys = &config.auth.keys;
    let enabled_keys = keys.iter().filter(|key| key.enabled).count();
    let mut line = format!("client keys  {}", keys.len());
    if enabled_keys != keys.len() {
        line.push_str(&format!(" ({} disabled)", keys.len() - enabled_keys));
    }
    line.push_str(if config.auth.required {
        ", authentication required"
    } else {
        ", authentication not required"
    });
    report.summary.push(line);
    for (index, key) in keys.iter().enumerate() {
        if key.enabled {
            unset(
                format!("auth.keys[{index}].key"),
                &key.key,
                &mut report.warnings,
            );
        }
    }
    if config.auth.required && enabled_keys == 0 {
        report.warnings.push(
            "auth.required is true but no client key is configured (or enabled): \
             every API request will be refused"
                .to_string(),
        );
    }
    if !config.auth.required && !local_only {
        report.warnings.push(format!(
            "auth.required is false and server.host is {}: anyone who can reach the \
             gateway can use it",
            config.server.host.trim()
        ));
    }

    // Providers.
    let enabled_providers = config.providers.iter().filter(|p| p.enabled).count();
    let mut line = format!("providers    {}", config.providers.len());
    if enabled_providers != config.providers.len() {
        line.push_str(&format!(
            " ({} disabled)",
            config.providers.len() - enabled_providers
        ));
    }
    report.summary.push(line);
    let width = config
        .providers
        .iter()
        .map(|p| p.name.chars().count())
        .max()
        .unwrap_or(0);
    for (index, provider) in config.providers.iter().enumerate() {
        let credentials = provider.all_credentials();
        let usable = credentials.iter().filter(|c| !c.disabled).count();
        let keyless = !provider.kind.needs_credentials()
            && credentials
                .iter()
                .all(|c| c.api_key.trim().is_empty() && c.service_account_file.trim().is_empty());
        let mut line = format!(
            "  {:width$}  {:13}  ",
            provider.name,
            provider.kind.as_str(),
            width = width
        );
        if keyless {
            line.push_str("no key needed");
        } else {
            line.push_str(&plural(credentials.len(), "credential"));
            if usable != credentials.len() {
                line.push_str(&format!(" ({} disabled)", credentials.len() - usable));
            }
        }
        if !provider.models.is_empty() {
            line.push_str(&format!(", {}", plural(provider.models.len(), "model")));
        }
        if !provider.enabled {
            line.push_str(", disabled");
        }
        report.summary.push(line);

        if !provider.enabled {
            continue;
        }
        let path = format!("providers[{index}]");
        if provider.kind.needs_credentials() && usable == 0 {
            report.warnings.push(if credentials.is_empty() {
                format!(
                    "provider `{}` has no credentials: add api_keys (or credentials)",
                    provider.name
                )
            } else {
                format!("provider `{}`: every credential is disabled", provider.name)
            });
        }
        for (position, key) in provider.api_keys.iter().enumerate() {
            unset(
                format!("{path}.api_keys[{position}]"),
                key,
                &mut report.warnings,
            );
        }
        for (position, credential) in provider.credentials.iter().enumerate() {
            if credential.disabled {
                continue;
            }
            unset(
                format!("{path}.credentials[{position}].api_key"),
                &credential.api_key,
                &mut report.warnings,
            );
            let file = credential.service_account_file.trim();
            if let Some(dir) = config_dir
                && !file.is_empty()
                && !dir.join(file).is_file()
            {
                report.warnings.push(format!(
                    "{path}.credentials[{position}].service_account_file: the file does not exist"
                ));
            }
        }
    }
    if enabled_providers == 0 {
        report
            .warnings
            .push("no provider is configured (or enabled): there is no model to serve".to_string());
    }

    report
        .summary
        .push(format!("aliases      {}", config.aliases.len()));
    report
}

/// The text `check` prints for a valid configuration: `ok`, the summary and
/// the warnings.
pub fn render(path: &Path, report: &CheckReport) -> String {
    let mut out = format!("ok: {} is a valid configuration\n", path.display());
    for line in &report.summary {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    if !report.warnings.is_empty() {
        out.push_str(&format!("{}:\n", plural(report.warnings.len(), "warning")));
        for warning in &report.warnings {
            out.push_str("  - ");
            out.push_str(warning);
            out.push('\n');
        }
    }
    out
}

/// Reads a configuration file for `check` and `serve`.
pub fn read_config_text(path: &Path) -> Result<String, CliError> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
            CliError::usage(format!(
                "the configuration file {} is not valid UTF-8 text",
                path.display()
            ))
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(CliError::usage(format!(
            "there is no configuration file at {}; run `switchyard init` to create one",
            path.display()
        ))),
        Err(error) => Err(CliError::failure(format!(
            "cannot read the configuration file {}: {error}",
            path.display()
        ))),
    }
}

/// Runs `check` on `path` and returns what to print on stdout. An invalid
/// file is an error with exit code 2 that lists every issue.
pub fn run(path: &Path, env: EnvLookup<'_>) -> Result<String, CliError> {
    let text = read_config_text(path)?;
    let config = validate_text(&text).map_err(|issues| CliError::invalid_config(path, &issues))?;
    let shown = absolute(path);
    let report = inspect(&config, env, shown.parent());
    Ok(render(&shown, &report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EXIT_FAILURE, EXIT_USAGE};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn inspect_text(text: &str, env: EnvLookup<'_>) -> CheckReport {
        let config = validate_text(text).expect("valid test configuration");
        inspect(&config, env, None)
    }

    const FULL: &str = r#"
[server]
host = "127.0.0.1"

[admin]
secret = "a-long-admin-secret-value"

[[auth.keys]]
key = "sy-one"
name = "laptop"

[[auth.keys]]
key = "sy-two"
enabled = false

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["sk-live-AAAAAAAAAAAAAAAAAAAA", "sk-live-BBBBBBBBBBBBBBBBBBBB"]

[[providers]]
name = "local"
kind = "openai-compat"
base_url = "http://127.0.0.1:11434/v1"

[[providers.models]]
id = "llama3.3"

[[providers]]
name = "claude"
kind = "anthropic"
enabled = false

[[aliases]]
name = "smart"
targets = ["gpt-5"]
"#;

    #[test]
    fn a_healthy_configuration_has_a_summary_and_no_warnings() {
        let report = inspect_text(FULL, &no_env);
        assert_eq!(report.warnings, Vec::<String>::new());
        let summary = report.summary.join("\n");
        assert!(
            summary.contains("listen       127.0.0.1:8317 (http)"),
            "{summary}"
        );
        assert!(summary.contains("admin        secret set, local connections only"));
        assert!(summary.contains("client keys  2 (1 disabled), authentication required"));
        assert!(summary.contains("providers    3 (1 disabled)"));
        assert!(
            summary.contains("openai  openai         2 credentials"),
            "{summary}"
        );
        assert!(
            summary.contains("local   openai-compat  no key needed, 1 model"),
            "{summary}"
        );
        assert!(
            summary.contains("claude  anthropic      0 credentials, disabled"),
            "{summary}"
        );
        assert!(summary.contains("aliases      1"));
    }

    #[test]
    fn output_never_contains_a_secret() {
        let config = validate_text(FULL).unwrap();
        let text = render(
            Path::new("switchyard.toml"),
            &inspect(&config, &no_env, None),
        );
        for secret in [
            "a-long-admin-secret-value",
            "sy-one",
            "sy-two",
            "sk-live-AAAAAAAAAAAAAAAAAAAA",
            "sk-live-BBBBBBBBBBBBBBBBBBBB",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.starts_with("ok: switchyard.toml is a valid configuration\n"));
        assert!(!text.contains("warning"));
    }

    #[test]
    fn an_empty_file_warns_about_everything_missing() {
        let report = inspect_text("", &no_env);
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("no admin secret is set"), "{warnings}");
        assert!(
            warnings.contains("no client key is configured"),
            "{warnings}"
        );
        assert!(warnings.contains("no provider is configured"), "{warnings}");
        assert_eq!(report.warnings.len(), 3);
        let text = render(Path::new("x.toml"), &report);
        assert!(text.contains("3 warnings:\n  - "), "{text}");
    }

    #[test]
    fn unset_environment_references_are_reported_by_path() {
        let text = r#"
[admin]
secret = "env:SY_TEST_ADMIN"

[[auth.keys]]
key = "${SY_TEST_CLIENT}"

[[providers]]
name = "openai"
kind = "openai"
api_keys = ["env:SY_TEST_SET", "env:SY_TEST_UNSET"]

[[providers.credentials]]
api_key = "env:SY_TEST_OTHER"

[[providers.credentials]]
api_key = "env:SY_TEST_DISABLED"
disabled = true
"#;
        let env = |name: &str| (name == "SY_TEST_SET").then(|| "sk-x".to_string());
        let report = inspect_text(text, &env);
        assert_eq!(
            report.warnings,
            vec![
                "admin.secret refers to the environment variable SY_TEST_ADMIN, which is not set",
                "auth.keys[0].key refers to the environment variable SY_TEST_CLIENT, which is not set",
                "providers[0].api_keys[1] refers to the environment variable SY_TEST_UNSET, which is not set",
                "providers[0].credentials[0].api_key refers to the environment variable SY_TEST_OTHER, which is not set",
            ]
        );

        // A blank value counts as unset, like the gateway treats it.
        let blank = |_: &str| Some("  ".to_string());
        assert_eq!(inspect_text(text, &blank).warnings.len(), 5);
    }

    #[test]
    fn the_admin_secret_may_come_from_the_environment() {
        let env = |name: &str| (name == ADMIN_SECRET_ENV).then(|| "from-env".to_string());
        let report = inspect_text(
            "[admin]\nsecret = \"env:NOT_SET_ANYWHERE\"\n[[auth.keys]]\nkey = \"k\"\n[[providers]]\nname = \"m\"\nkind = \"mock\"\n",
            &env,
        );
        assert_eq!(report.warnings, Vec::<String>::new());
        assert!(
            report
                .summary
                .iter()
                .any(|line| line.contains("secret from SWITCHYARD_ADMIN_SECRET"))
        );
    }

    #[test]
    fn providers_without_credentials() {
        let text = r#"
[admin]
secret = "s"
[[auth.keys]]
key = "k"

[[providers]]
name = "empty"
kind = "gemini"

[[providers]]
name = "resting"
kind = "anthropic"
[[providers.credentials]]
api_key = "sk-ant-xxxxxxxxxxxxxxxx"
disabled = true

[[providers]]
name = "off"
kind = "openai"
enabled = false
"#;
        let report = inspect_text(text, &no_env);
        assert_eq!(
            report.warnings,
            vec![
                "provider `empty` has no credentials: add api_keys (or credentials)",
                "provider `resting`: every credential is disabled",
            ]
        );
    }

    #[test]
    fn remote_admin_and_open_access_on_a_public_address() {
        let text = r#"
[server]
host = "0.0.0.0"
[admin]
secret = "s"
allow_remote = true
[auth]
required = false
[[providers]]
name = "m"
kind = "mock"
"#;
        let report = inspect_text(text, &no_env);
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("admin.allow_remote is true"));
        assert!(report.warnings[1].contains("auth.required is false"));

        // The same settings on a loopback address are fine.
        let local = text.replace("0.0.0.0", "127.0.0.1");
        assert_eq!(inspect_text(&local, &no_env).warnings, Vec::<String>::new());

        // The environment switch counts like the setting.
        let by_env = text.replace("allow_remote = true", "allow_remote = false");
        assert_eq!(inspect_text(&by_env, &no_env).warnings.len(), 1);
        let env = |name: &str| (name == ADMIN_ALLOW_REMOTE_ENV).then(|| "TRUE".to_string());
        assert_eq!(inspect_text(&by_env, &env).warnings.len(), 2);
    }

    #[test]
    fn named_files_are_checked_when_the_directory_is_known() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cert.pem"), "x").unwrap();
        let text = r#"
[server.tls]
cert = "cert.pem"
key = "key.pem"
[admin]
secret = "s"
[[auth.keys]]
key = "k"
[[providers]]
name = "vx"
kind = "vertex"
[[providers.credentials]]
service_account_file = "sa.json"
"#;
        let config = validate_text(text).unwrap();
        let report = inspect(&config, &no_env, Some(dir.path()));
        assert_eq!(
            report.warnings,
            vec![
                "server.tls.key: the file does not exist",
                "providers[0].credentials[0].service_account_file: the file does not exist",
            ]
        );
        assert!(report.summary[0].ends_with("(https)"));
        assert!(inspect(&config, &no_env, None).warnings.is_empty());
    }

    #[test]
    fn run_reports_valid_invalid_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good config.toml");
        std::fs::write(&good, FULL).unwrap();
        let out = run(&good, &no_env).unwrap();
        assert!(out.starts_with("ok: "), "{out}");
        assert!(out.contains("good config.toml"));

        let bad = dir.path().join("bad.toml");
        std::fs::write(
            &bad,
            "[server]\nport = 0\n[[providers]]\nname = \"Bad Name\"\nkind = \"openai-compat\"\n",
        )
        .unwrap();
        let error = run(&bad, &no_env).unwrap_err();
        assert_eq!(error.code, EXIT_USAGE);
        assert!(error.message.contains("bad.toml"));
        for line in [
            "\n  server.port: must be between 1 and 65535",
            "\n  providers[0].name: ",
            "\n  providers[0].base_url: ",
        ] {
            assert!(error.message.contains(line), "{}", error.message);
        }

        let syntax = dir.path().join("syntax.toml");
        std::fs::write(&syntax, "[server\n").unwrap();
        let error = run(&syntax, &no_env).unwrap_err();
        assert_eq!(error.code, EXIT_USAGE);
        assert!(
            error.message.contains("\n  line 1, column "),
            "{}",
            error.message
        );

        let missing = run(&dir.path().join("nope.toml"), &no_env).unwrap_err();
        assert_eq!(missing.code, EXIT_USAGE);
        assert!(missing.message.contains("switchyard init"));

        let binary = dir.path().join("binary.toml");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(run(&binary, &no_env).unwrap_err().code, EXIT_USAGE);

        // A directory cannot be read as a file.
        let unreadable = run(dir.path(), &no_env).unwrap_err();
        assert!(matches!(unreadable.code, EXIT_FAILURE | EXIT_USAGE));
    }

    #[test]
    fn a_pasted_key_in_the_wrong_place_is_not_echoed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leak.toml");
        let secret = "sk-live-0123456789abcdefghijklmnop";
        std::fs::write(
            &path,
            format!("[[providers]]\nname = \"a\"\nkind = \"openai\"\napi_keys = \"{secret}\"\n"),
        )
        .unwrap();
        let error = run(&path, &no_env).unwrap_err();
        assert!(!error.message.contains(secret), "{}", error.message);
    }

    #[test]
    fn secret_references() {
        assert_eq!(secret_reference("env:FOO"), Some("FOO"));
        assert_eq!(secret_reference(" ${ BAR } "), Some("BAR"));
        assert_eq!(secret_reference("sk-literal"), None);
        assert_eq!(secret_reference("${unterminated"), None);
    }
}
