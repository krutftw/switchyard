use crate::{
    AdapterError, ProfileBinding, Result, account_process::AccountProcess, discovery, types::now_ms,
};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const CODEX_ARGS: &[&str] = &[
    "app-server",
    "--stdio",
    "-c",
    "sandbox_mode=\"read-only\"",
    "-c",
    "approval_policy=\"on-request\"",
    "-c",
    "approvals_reviewer=\"user\"",
];
const PROBE_TIMEOUT: Duration = Duration::from_millis(7500);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Serialize)]
pub struct AccountSnapshot {
    pub auth_status: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub usage: AccountUsage,
    pub checked_at_ms: u64,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountUsage {
    pub status: String,
    pub windows: Vec<AccountUsageWindow>,
    pub ordinary_usage_allowed: Option<bool>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountUsageWindow {
    pub id: String,
    pub name: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoginJob {
    pub id: String,
    pub profile_id: String,
    pub status: String,
    pub auth_url: Option<String>,
    pub user_code: Option<String>,
    pub message: Option<String>,
    pub updated_at_ms: u64,
}

struct LoginEntry {
    view: Arc<Mutex<LoginJob>>,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

struct AccountInner {
    jobs: Mutex<BTreeMap<String, LoginEntry>>,
    slots: Arc<Semaphore>,
    shutdown: CancellationToken,
}

impl Drop for AccountInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Clone)]
pub struct AccountRuntime {
    inner: Arc<AccountInner>,
}

impl Default for AccountRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AccountInner {
                jobs: Mutex::new(BTreeMap::new()),
                slots: Arc::new(Semaphore::new(4)),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub async fn inspect(&self, binding: &ProfileBinding) -> Result<AccountSnapshot> {
        crate::profile::validate(binding, &binding.agent_id)?;
        if self.inner.shutdown.is_cancelled() {
            return Err(AdapterError::Unavailable(
                "Account runtime is shutting down.".into(),
            ));
        }
        if self.inner.jobs.lock().values().any(|e| {
            let j = e.view.lock();
            j.profile_id == binding.id && login_active(&j.status)
        }) {
            return Err(AdapterError::Conflict(
                "Finish or cancel this profile's sign-in before refreshing.".into(),
            ));
        }
        let _slot = self.inner.slots.clone().try_acquire_owned().map_err(|_| {
            AdapterError::Limit("Four account operations are already active.".into())
        })?;
        let executable = discovery::find_cli(&binding.agent_id).ok_or_else(|| {
            AdapterError::Unavailable(
                "Install this agent's native CLI before checking its account.".into(),
            )
        })?;
        let mut process = AccountProcess::spawn(
            executable,
            if binding.agent_id == "codex" {
                CODEX_ARGS
            } else {
                &["auth", "status", "--json"]
            },
            binding,
            false,
        )?;
        let result = tokio::select! {
            _ = self.inner.shutdown.cancelled() => Err(AdapterError::Unavailable("Account runtime is shutting down.".into())),
            outcome = tokio::time::timeout(PROBE_TIMEOUT, inspect_process(&mut process, binding)) => outcome.unwrap_or_else(|_| Err(AdapterError::Unavailable("The account check timed out. Try refreshing again.".into()))),
        };
        process.stop().await;
        result
    }

    /// Registers and spawns synchronously: dropping the returned future before it
    /// is polled cannot create a hidden login. There is no await before the job is returned.
    pub async fn start_login(&self, binding: ProfileBinding) -> Result<LoginJob> {
        crate::profile::validate(&binding, &binding.agent_id)?;
        if !binding.managed {
            return Err(AdapterError::Invalid(
                "Existing CLI profiles are read-only. Create a managed profile to sign in.".into(),
            ));
        }
        if self.inner.shutdown.is_cancelled() {
            return Err(AdapterError::Unavailable(
                "Account runtime is shutting down.".into(),
            ));
        }
        let executable = discovery::find_cli(&binding.agent_id).ok_or_else(|| {
            AdapterError::Unavailable("Install this agent's native CLI before signing in.".into())
        })?;
        let slot = self.inner.slots.clone().try_acquire_owned().map_err(|_| {
            AdapterError::Limit("Four account operations are already active.".into())
        })?;
        let mut jobs = self.inner.jobs.lock();
        if jobs.values().any(|e| login_active(&e.view.lock().status)) {
            return Err(AdapterError::Conflict("Finish or cancel the current sign-in before starting another. Signed-in profiles can be used concurrently.".into()));
        }
        if jobs.len() >= 64 {
            let remove = jobs
                .iter()
                .find(|(_, e)| {
                    !login_active(&e.view.lock().status)
                        && e.task.as_ref().is_none_or(|t| t.is_finished())
                })
                .map(|(id, _)| id.clone());
            if let Some(id) = remove {
                jobs.remove(&id);
            } else {
                return Err(AdapterError::Limit(
                    "Too many retained sign-in jobs.".into(),
                ));
            }
        }
        let view = LoginJob {
            id: format!("login_{}", uuid::Uuid::new_v4().simple()),
            profile_id: binding.id.clone(),
            status: "starting".into(),
            auth_url: None,
            user_code: None,
            message: None,
            updated_at_ms: now_ms(),
        };
        let state = Arc::new(Mutex::new(view.clone()));
        let cancel = self.inner.shutdown.child_token();
        let task_state = state.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let _slot = slot;
            run_login(executable, binding, task_state, task_cancel).await;
        });
        jobs.insert(
            view.id.clone(),
            LoginEntry {
                view: state,
                cancel,
                task: Some(task),
            },
        );
        Ok(view)
    }

    pub fn login(&self, id: &str) -> Result<LoginJob> {
        self.inner
            .jobs
            .lock()
            .get(id)
            .map(|e| e.view.lock().clone())
            .ok_or_else(|| AdapterError::NotFound("Sign-in job not found.".into()))
    }

    pub async fn cancel_login(&self, id: &str) -> Result<LoginJob> {
        let task = {
            let mut jobs = self.inner.jobs.lock();
            let entry = jobs
                .get_mut(id)
                .ok_or_else(|| AdapterError::NotFound("Sign-in job not found.".into()))?;
            entry.cancel.cancel();
            entry.task.take()
        };
        if let Some(task) = task {
            let _ = task.await;
        }
        self.login(id)
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let tasks: Vec<_> = self
            .inner
            .jobs
            .lock()
            .values_mut()
            .filter_map(|e| e.task.take())
            .collect();
        for task in tasks {
            let _ = task.await;
        }
    }
}

fn login_active(status: &str) -> bool {
    matches!(status, "starting" | "awaiting_user")
}

fn unavailable_usage(message: &str) -> AccountUsage {
    AccountUsage {
        status: "unavailable".into(),
        windows: Vec::new(),
        ordinary_usage_allowed: None,
        message: Some(message.into()),
    }
}

async fn inspect_process(
    process: &mut AccountProcess,
    binding: &ProfileBinding,
) -> Result<AccountSnapshot> {
    if binding.agent_id == "claude" {
        let mut text = String::new();
        while let Some((stderr, line)) = process.line().await? {
            if !stderr {
                text.push_str(&line);
            }
            if text.len() > 256 * 1024 {
                return Err(AdapterError::Limit(
                    "Claude account status exceeded its output limit.".into(),
                ));
            }
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| AdapterError::Protocol("Claude returned an unsupported account status format. Check sign-in in its official CLI.".into()))?;
        return Ok(claude_snapshot(&value));
    }
    process.initialize(binding).await?;
    let account = process
        .rpc(2, "account/read", json!({"refreshToken":false}))
        .await?;
    let mut result = codex_snapshot(&account);
    if result.auth_status == "signed_in" {
        result.usage = match process
            .rpc(
                3,
                "account/rateLimits/read",
                json!({"excludeResetCreditDetails":true,"supportsLunaReserve":false}),
            )
            .await
        {
            Ok(value) => codex_usage(&value),
            Err(_) => AccountUsage {
                status: "error".into(),
                message: Some(
                    "Codex did not provide subscription usage. Refresh to try again.".into(),
                ),
                ..unavailable_usage("")
            },
        };
    }
    Ok(result)
}

fn codex_snapshot(value: &Value) -> AccountSnapshot {
    let account = value.get("account");
    let auth_status = if account.is_some_and(Value::is_null) {
        "signed_out"
    } else if account
        .and_then(|a| a.get("type"))
        .and_then(Value::as_str)
        .is_some()
    {
        "signed_in"
    } else {
        "unknown"
    };
    AccountSnapshot {
        auth_status: auth_status.into(),
        email: account.and_then(|a| safe_email(a.get("email"))),
        plan: account.and_then(|a| safe_text(a.get("planType"), 80)),
        usage: unavailable_usage("Usage has not been provided by Codex."),
        checked_at_ms: now_ms(),
        message: None,
    }
}

fn claude_snapshot(value: &Value) -> AccountSnapshot {
    let auth_status = match value.get("loggedIn").and_then(Value::as_bool) {
        Some(true) => "signed_in",
        Some(false) => "signed_out",
        None => "unknown",
    };
    AccountSnapshot {
        auth_status: auth_status.into(),
        email: safe_email(value.get("email")),
        plan: safe_text(value.get("subscriptionType"), 80),
        usage: unavailable_usage(
            "Claude Code does not expose subscription quota through its documented auth-status command. View usage in Claude Code or your account.",
        ),
        checked_at_ms: now_ms(),
        message: (auth_status == "unknown")
            .then(|| "This Claude CLI did not provide a recognized sign-in status.".into()),
    }
}

fn safe_text(value: Option<&Value>, limit: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

fn safe_email(value: Option<&Value>) -> Option<String> {
    safe_text(value, 254)
        .filter(|s| s.matches('@').count() == 1 && !s.chars().any(char::is_whitespace))
}

fn codex_usage(value: &Value) -> AccountUsage {
    let mut windows = Vec::new();
    let snapshots: Vec<(String, &Value)> = if let Some(map) = value
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
    {
        map.iter().take(16).map(|(id, v)| (id.clone(), v)).collect()
    } else {
        value
            .get("rateLimits")
            .map(|v| vec![("codex".into(), v)])
            .unwrap_or_default()
    };
    for (index, (id, snapshot)) in snapshots.into_iter().enumerate() {
        let id = if id.len() <= 100
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            id
        } else {
            format!("limit-{index}")
        };
        let label = safe_text(snapshot.get("limitName"), 100).unwrap_or_else(|| id.clone());
        for key in ["primary", "secondary"] {
            let Some(window) = snapshot.get(key).filter(|v| v.is_object()) else {
                continue;
            };
            let used = window
                .get("usedPercent")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0);
            windows.push(AccountUsageWindow {
                id: format!("{id}:{key}"),
                name: format!("{label} {key}"),
                used_percent: used,
                remaining_percent: used.map(|v| (100.0 - v).clamp(0.0, 100.0)),
                window_minutes: window.get("windowDurationMins").and_then(Value::as_u64),
                resets_at: window.get("resetsAt").and_then(Value::as_u64),
            });
        }
    }
    let available = windows.iter().any(|w| w.used_percent.is_some());
    AccountUsage {
        status: if available {
            "available"
        } else {
            "unavailable"
        }
        .into(),
        windows,
        ordinary_usage_allowed: value.get("ordinaryUsageAllowed").and_then(Value::as_bool),
        message: (!available).then(|| "Codex returned no percentage-based quota windows.".into()),
    }
}

async fn run_login(
    executable: std::path::PathBuf,
    binding: ProfileBinding,
    state: Arc<Mutex<LoginJob>>,
    cancel: CancellationToken,
) {
    if binding.agent_id == "claude" {
        let mut probe = match AccountProcess::spawn(
            executable.clone(),
            &["auth", "status", "--json"],
            &binding,
            false,
        ) {
            Ok(probe) => probe,
            Err(_) => {
                set_login(
                    &state,
                    "failed",
                    Some("Claude sign-in status could not be checked safely."),
                );
                return;
            }
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => None,
            outcome = tokio::time::timeout(PROBE_TIMEOUT, inspect_process(&mut probe,&binding)) => Some(outcome),
        };
        probe.stop().await;
        match result {
            None => {
                set_login(&state, "cancelled", Some("Sign-in was cancelled."));
                return;
            }
            Some(Ok(Ok(snapshot))) if snapshot.auth_status == "signed_out" => {}
            _ => {
                set_login(
                    &state,
                    "failed",
                    Some(
                        "Sign-in requires a confirmed signed-out profile. Refresh status or create another profile; existing credentials will not be replaced.",
                    ),
                );
                return;
            }
        }
    }
    let mut process = match AccountProcess::spawn(
        executable,
        if binding.agent_id == "codex" {
            CODEX_ARGS
        } else {
            &["auth", "login", "--claudeai"]
        },
        &binding,
        binding.agent_id == "claude",
    ) {
        Ok(process) => process,
        Err(_) => {
            set_login(
                &state,
                "failed",
                Some("The official account CLI could not start."),
            );
            return;
        }
    };
    let mut remote_login_id = None;
    let result = tokio::select! {
        _ = cancel.cancelled() => None,
        outcome = tokio::time::timeout(LOGIN_TIMEOUT, login_process(&mut process,&binding,&state,&mut remote_login_id)) => Some(outcome.unwrap_or_else(|_| Err(AdapterError::Unavailable("Sign-in timed out after ten minutes. Start a new sign-in to try again.".into())))),
    };
    if result.is_none()
        && binding.agent_id == "codex"
        && let Some(login_id) = remote_login_id
    {
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            process.rpc(4, "account/login/cancel", json!({"loginId":login_id})),
        )
        .await;
    }
    process.stop().await;
    match result {
        None => set_login(
            &state,
            "cancelled",
            Some("Sign-in was cancelled. Existing credentials, if any, were not removed."),
        ),
        Some(Ok(())) => set_login(&state, "completed", None),
        Some(Err(error)) => set_login(&state, "failed", Some(&error.to_string())),
    }
}

async fn login_process(
    process: &mut AccountProcess,
    binding: &ProfileBinding,
    state: &Arc<Mutex<LoginJob>>,
    remote_login_id: &mut Option<String>,
) -> Result<()> {
    if binding.agent_id == "claude" {
        set_login(
            state,
            "awaiting_user",
            Some("Complete sign-in in the official Claude Code browser flow."),
        );
        while let Some((_stderr, line)) = process.line().await? {
            for word in line.split_whitespace() {
                let candidate = word.trim_matches(['\"', '\'', '(', ')', '<', '>', ',']);
                if let Some(url) = official_auth_url("claude", candidate) {
                    let mut job = state.lock();
                    job.auth_url = Some(url);
                    job.updated_at_ms = now_ms();
                }
            }
        }
        let status =
            process.child.wait().await.map_err(|_| {
                AdapterError::Protocol("Claude sign-in exited unexpectedly.".into())
            })?;
        return if status.success() {
            Ok(())
        } else {
            Err(AdapterError::Unavailable(
                "Claude sign-in was not completed. Retry or use the official CLI.".into(),
            ))
        };
    }
    process.initialize(binding).await?;
    let account = process
        .rpc(2, "account/read", json!({"refreshToken":false}))
        .await?;
    if codex_snapshot(&account).auth_status != "signed_out" {
        return Err(AdapterError::Conflict("Sign-in requires a confirmed signed-out profile. Existing credentials will not be replaced.".into()));
    }
    let response = process
        .rpc(3, "account/login/start", json!({"type":"chatgpt"}))
        .await?;
    let login_id = safe_text(response.get("loginId"), 200)
        .ok_or_else(|| AdapterError::Protocol("Codex returned no sign-in identifier.".into()))?;
    *remote_login_id = Some(login_id.clone());
    let url = response
        .get("authUrl")
        .and_then(Value::as_str)
        .and_then(|s| official_auth_url("codex", s))
        .ok_or_else(|| {
            AdapterError::Protocol(
                "Codex returned an unrecognized sign-in URL. Use the official CLI to sign in."
                    .into(),
            )
        })?;
    {
        let mut job = state.lock();
        job.status = "awaiting_user".into();
        job.auth_url = Some(url);
        job.updated_at_ms = now_ms();
    }
    loop {
        let message = process.message().await?;
        if message.get("method").and_then(Value::as_str) != Some("account/login/completed") {
            continue;
        }
        let params = message.get("params").unwrap_or(&Value::Null);
        if params.get("loginId").and_then(Value::as_str) != Some(login_id.as_str()) {
            continue;
        }
        return if params.get("success").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(AdapterError::Unavailable(
                "Codex sign-in was not completed. Start a new sign-in to try again.".into(),
            ))
        };
    }
}

fn set_login(state: &Arc<Mutex<LoginJob>>, status: &str, message: Option<&str>) {
    let mut job = state.lock();
    job.status = status.into();
    job.message = message.map(str::to_owned);
    job.updated_at_ms = now_ms();
    if !login_active(status) {
        job.auth_url = None;
        job.user_code = None;
    }
}

/// Exact official hosts only. No caller-supplied URLs, credentials, or arbitrary schemes.
pub fn official_auth_url(agent: &str, input: &str) -> Option<String> {
    if input.len() > 16 * 1024 || input.chars().any(char::is_control) {
        return None;
    }
    let url = url::Url::parse(input).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
    {
        return None;
    }
    let host = url.host_str()?;
    let allowed = match agent {
        "codex" => matches!(host, "auth.openai.com" | "chatgpt.com"),
        "claude" => host == "claude.ai",
        _ => false,
    };
    if !allowed
        || url.query_pairs().any(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "access_token" | "id_token" | "refresh_token" | "api_key"
            )
        })
    {
        return None;
    }
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::OnceLock};

    fn fixture() -> PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap().keep();
            let path = dir.join(if cfg!(windows) {
                "account-fixture.exe"
            } else {
                "account-fixture"
            });
            let output = std::process::Command::new("rustc")
                .args(["--edition=2024"])
                .arg(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/account_child.rs"),
                )
                .arg("-o")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture compile failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            path
        })
        .clone()
    }

    fn binding(dir: &tempfile::TempDir, agent: &str) -> ProfileBinding {
        ProfileBinding {
            id: "fixture-profile".into(),
            name: "Fixture".into(),
            agent_id: agent.into(),
            home: dir.path().canonicalize().unwrap(),
            managed: true,
        }
    }

    #[tokio::test]
    async fn account_transport_reads_only_typed_fields_and_never_starts_turns() {
        let dir = tempfile::tempdir().unwrap();
        let binding = binding(&dir, "codex");
        let mut process = AccountProcess::spawn(fixture(), &["inspect"], &binding, false).unwrap();
        let snapshot = tokio::time::timeout(
            Duration::from_secs(5),
            inspect_process(&mut process, &binding),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.auth_status, "signed_in");
        assert_eq!(snapshot.usage.windows[0].remaining_percent, Some(27.0));
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("fixture-do-not-forward")
        );
        process.stop().await;
        assert!(process.child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn account_transport_rejects_wrong_home_invalid_json_and_oversized_frames() {
        let dir = tempfile::tempdir().unwrap();
        let binding = binding(&dir, "codex");
        for scenario in ["wrong_home", "malformed", "oversized"] {
            let mut process =
                AccountProcess::spawn(fixture(), &[scenario], &binding, false).unwrap();
            assert!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    inspect_process(&mut process, &binding)
                )
                .await
                .unwrap()
                .is_err(),
                "{scenario}"
            );
            process.stop().await;
            assert!(process.child.try_wait().unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn login_protocol_requires_signed_out_and_matching_completion() {
        let dir = tempfile::tempdir().unwrap();
        let binding = binding(&dir, "codex");
        for scenario in ["inspect", "login_success"] {
            let mut process =
                AccountProcess::spawn(fixture(), &[scenario], &binding, false).unwrap();
            let state = Arc::new(Mutex::new(LoginJob {
                id: "fixture".into(),
                profile_id: binding.id.clone(),
                status: "starting".into(),
                auth_url: None,
                user_code: None,
                message: None,
                updated_at_ms: 0,
            }));
            let mut remote = None;
            let outcome = tokio::time::timeout(
                Duration::from_secs(5),
                login_process(&mut process, &binding, &state, &mut remote),
            )
            .await
            .unwrap();
            assert_eq!(outcome.is_ok(), scenario == "login_success");
            if scenario == "login_success" {
                assert_eq!(remote.as_deref(), Some("fixture-login"));
                assert!(state.lock().auth_url.is_some());
            }
            process.stop().await;
        }
    }

    #[tokio::test]
    async fn pending_login_can_be_cancelled_and_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let binding = binding(&dir, "codex");
        let mut process =
            AccountProcess::spawn(fixture(), &["login_wait"], &binding, false).unwrap();
        let state = Arc::new(Mutex::new(LoginJob {
            id: "fixture".into(),
            profile_id: binding.id.clone(),
            status: "starting".into(),
            auth_url: None,
            user_code: None,
            message: None,
            updated_at_ms: 0,
        }));
        let mut remote = None;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(250),
                login_process(&mut process, &binding, &state, &mut remote)
            )
            .await
            .is_err()
        );
        assert_eq!(remote.as_deref(), Some("fixture-login"));
        let canceled = process
            .rpc(4, "account/login/cancel", json!({"loginId":remote}))
            .await
            .unwrap();
        assert_eq!(canceled["status"], "canceled");
        process.stop().await;
        assert!(process.child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn claude_status_fixture_has_no_quota_or_untyped_secret_output() {
        let dir = tempfile::tempdir().unwrap();
        let binding = binding(&dir, "claude");
        let mut process =
            AccountProcess::spawn(fixture(), &["claude_status"], &binding, false).unwrap();
        let snapshot = tokio::time::timeout(
            Duration::from_secs(5),
            inspect_process(&mut process, &binding),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.auth_status, "signed_out");
        assert_eq!(snapshot.usage.status, "unavailable");
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("fixture-do-not-forward")
        );
        process.stop().await;
    }
    #[test]
    fn usage_maps_used_percent_and_preserves_unknown() {
        let usage = codex_usage(
            &json!({"rateLimits":{"primary":{"usedPercent":91,"windowDurationMins":300,"resetsAt":123},"secondary":null},"ordinaryUsageAllowed":null}),
        );
        assert_eq!(usage.windows[0].remaining_percent, Some(9.0));
        assert_eq!(usage.ordinary_usage_allowed, None);
        assert_eq!(codex_usage(&json!({"rateLimits":{}})).status, "unavailable");
        assert!(codex_usage(&json!({"rateLimits":{}})).windows.is_empty());
    }
    #[test]
    fn multiple_limit_ids_and_missing_values_are_retained() {
        let usage = codex_usage(
            &json!({"rateLimits":{"primary":{"usedPercent":1}},"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":125},"secondary":{"usedPercent":null}},"other":{"primary":{"usedPercent":5}}}}),
        );
        assert_eq!(usage.windows.len(), 3);
        assert_eq!(usage.windows[0].remaining_percent, Some(0.0));
        assert_eq!(usage.windows[1].remaining_percent, None);
    }
    #[test]
    fn account_views_never_serialize_unrecognized_fields() {
        let view = codex_snapshot(
            &json!({"account":{"type":"chatgpt","email":"person@example.test","planType":"pro","access_token":"never-public"},"secret":"never-public"}),
        );
        assert_eq!(view.auth_status, "signed_in");
        assert!(
            !serde_json::to_string(&view)
                .unwrap()
                .contains("never-public")
        );
        assert_eq!(
            claude_snapshot(&json!({"loggedIn":false})).auth_status,
            "signed_out"
        );
        assert_eq!(
            claude_snapshot(&json!({"newFormat":true})).auth_status,
            "unknown"
        );
        assert_eq!(
            claude_snapshot(&json!({"loggedIn":true})).usage.status,
            "unavailable"
        );
    }
    #[test]
    fn login_urls_require_exact_official_hosts() {
        assert!(
            official_auth_url(
                "codex",
                "https://auth.openai.com/oauth/authorize?state=test"
            )
            .is_some()
        );
        assert!(official_auth_url("claude", "https://claude.ai/oauth/authorize").is_some());
        for input in [
            "http://auth.openai.com/",
            "https://auth.openai.com.evil.test/",
            "https://user:pass@auth.openai.com/",
            "https://auth.openai.com:8443/",
            "https://auth.openai.com/?access_token=secret",
            "file:///tmp/x",
        ] {
            assert!(official_auth_url("codex", input).is_none(), "{input}");
        }
    }
}
