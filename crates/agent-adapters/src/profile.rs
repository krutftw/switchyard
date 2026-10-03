use crate::{AdapterError, ProfileBinding, Result};
use serde_json::Value;
use tokio::process::Command;

pub(crate) fn validate(profile: &ProfileBinding, agent: &str) -> Result<()> {
    if profile.agent_id != agent || !matches!(agent, "codex" | "claude") {
        return Err(AdapterError::Invalid(
            "Account profile does not match the selected agent.".into(),
        ));
    }
    if profile.managed && (!profile.home.is_absolute() || !profile.home.is_dir()) {
        return Err(AdapterError::Invalid(
            "Managed account directory is unavailable.".into(),
        ));
    }
    if !profile.managed && (profile.id != "system-codex" || agent != "codex") {
        return Err(AdapterError::Invalid(
            "Unknown existing CLI profile.".into(),
        ));
    }
    Ok(())
}

/// Scope all changes to the owned child. Never swap the host's environment or auth files.
pub(crate) fn configure(command: &mut Command, profile: Option<&ProfileBinding>) {
    // A parent Codex host's private app-server routing must not leak into our child.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        let internal = name.starts_with("CODEX_INTERNAL_") || name.starts_with("CODEX_APP_SERVER_");
        let managed = profile.is_some_and(|p| p.managed);
        let credential_override = managed
            && ([
                "CODEX_",
                "OPENAI_",
                "CHATGPT_",
                "AZURE_OPENAI_",
                "ANTHROPIC_",
                "CLAUDE_",
                "AWS_",
                "GOOGLE_",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
                || matches!(
                    name.as_str(),
                    "CLAUDECODE"
                        | "NODE_OPTIONS"
                        | "BUN_OPTIONS"
                        | "GOOGLE_APPLICATION_CREDENTIALS"
                ));
        if internal || credential_override {
            command.env_remove(key);
        }
    }
    if let Some(profile) = profile.filter(|p| p.managed) {
        command.env(
            if profile.agent_id == "codex" {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            },
            &profile.home,
        );
        if profile.agent_id == "codex" {
            command.args(["-c", "cli_auth_credentials_store=\"file\""]);
        }
    }
}

pub(crate) fn confirmed_home(result: &Value, profile: Option<&ProfileBinding>) -> bool {
    let Some(profile) = profile.filter(|p| p.managed) else {
        return true;
    };
    let Some(home) = result.get("codexHome").and_then(Value::as_str) else {
        return false;
    };
    match (std::fs::canonicalize(home), profile.home.canonicalize()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn managed_home_is_scoped_to_child_and_must_be_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let profile = ProfileBinding {
            id: "fixture".into(),
            name: "Fixture".into(),
            agent_id: "codex".into(),
            home: dir.path().canonicalize().unwrap(),
            managed: true,
        };
        let mut command = Command::new("fixture");
        configure(&mut command, Some(&profile));
        let home = command
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == "CODEX_HOME")
            .and_then(|(_, v)| v);
        assert_eq!(home, Some(profile.home.as_os_str()));
        assert!(confirmed_home(
            &json!({"codexHome":profile.home}),
            Some(&profile)
        ));
        assert!(!confirmed_home(&json!({}), Some(&profile)));
        assert!(!confirmed_home(
            &json!({"codexHome":"/wrong-profile"}),
            Some(&profile)
        ));
    }

    #[test]
    fn managed_home_cannot_be_deserialized_from_start_json() {
        let value = json!({"adapter_id":"codex","project_path":"/fixture","prompt":"test","command_id":"00000000-0000-0000-0000-000000000000","profile":{"home":"/attacker","managed":true}});
        assert!(serde_json::from_value::<crate::StartRequest>(value).is_err());
    }
}
