//! Private account profiles. Only labels and selections are persisted here;
//! official CLI processes own their credentials inside separate profile homes.
use crate::api::{AppState, body, no_query};
use crate::{AppError, Result};
use axum::Json;
use axum::extract::{Path as RoutePath, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use switchyard_agent_adapters::{
    AccountRuntime, AccountSnapshot, AccountUsage, AdapterManager, AdapterRun, LoginJob,
    ProfileBinding, StartRequest, launch_profile_terminal, official_auth_url,
};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const SYSTEM_CODEX: &str = "system-codex";
const MAX_PROFILES: usize = 32;
const MAX_MANIFEST: u64 = 128 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Defaults {
    codex: Option<String>,
    claude: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileRecord {
    id: String,
    agent_id: String,
    name: String,
    created_at_ms: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    profiles: Vec<ProfileRecord>,
    defaults: Defaults,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: 1,
            profiles: Vec::new(),
            defaults: Defaults {
                codex: Some(SYSTEM_CODEX.into()),
                claude: None,
            },
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct ProfileView {
    id: String,
    name: String,
    agent_id: String,
    managed: bool,
    read_only: bool,
    selected: bool,
    refreshing: bool,
    account: AccountSnapshot,
}

struct CachedProfile {
    account: AccountSnapshot,
    refreshing: bool,
    login_busy: bool,
    terminal_busy: bool,
    login: Option<LoginJob>,
}

impl Default for CachedProfile {
    fn default() -> Self {
        Self {
            account: unknown_account(
                "Refresh to read this account's current sign-in and usage status.",
            ),
            refreshing: false,
            login_busy: false,
            terminal_busy: false,
            login: None,
        }
    }
}

struct ProfileState {
    manifest: Manifest,
    disk_bytes: Option<Vec<u8>>,
    cache: BTreeMap<String, CachedProfile>,
}

struct Inner {
    data_dir: PathBuf,
    homes: PathBuf,
    state: Mutex<ProfileState>,
    runtime: AccountRuntime,
    reads: Arc<Semaphore>,
    stopped: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stopped.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct AccountManager {
    inner: Arc<Inner>,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn unknown_account(message: &str) -> AccountSnapshot {
    AccountSnapshot {
        auth_status: "unknown".into(),
        email: None,
        plan: None,
        usage: AccountUsage {
            status: "unavailable".into(),
            windows: Vec::new(),
            ordinary_usage_allowed: None,
            message: Some(message.into()),
        },
        checked_at_ms: 0,
        message: Some(message.into()),
    }
}

fn active_login(job: &LoginJob) -> bool {
    matches!(job.status.as_str(), "starting" | "awaiting_user")
}

fn valid_agent(agent: &str) -> bool {
    matches!(agent, "codex" | "claude")
}

fn checked_name(name: &str) -> Result<String> {
    if name.chars().any(char::is_control) {
        return Err(AppError::invalid(
            "Profile names cannot contain control characters.",
        ));
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(AppError::invalid("Use a profile name of 1–64 characters."));
    }
    Ok(name.to_owned())
}

fn managed_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}

fn selected<'a>(defaults: &'a Defaults, agent: &str) -> Option<&'a str> {
    match agent {
        "codex" => defaults.codex.as_deref(),
        "claude" => defaults.claude.as_deref(),
        _ => None,
    }
}

impl AccountManager {
    /// Filesystem and cached state only. The caller explicitly requests refreshes.
    pub(crate) fn open(data_dir: &Path) -> Result<Self> {
        let absolute = std::path::absolute(data_dir)
            .map_err(|_| AppError::invalid("Could not resolve the account data directory."))?;
        let data_dir = absolute
            .canonicalize()
            .map_err(|_| AppError::invalid("The account data directory must exist."))?;
        check_directory_chain(&data_dir)?;
        let homes = data_dir.join("accounts");
        crate::startup::private_directory(&homes)?;
        check_directory_chain(&homes)?;
        let disk_bytes = read_manifest(&data_dir.join("account-profiles.json"))?;
        let manifest: Manifest = match &disk_bytes {
            Some(bytes) => serde_json::from_slice(bytes).map_err(|_| AppError::invalid("Account profile metadata is invalid. Restore account-profiles.json before starting the app."))?,
            None => Manifest::default(),
        };
        validate_manifest(&manifest)?;
        for profile in &manifest.profiles {
            let home = homes.join(&profile.id);
            check_directory_chain(&home)?;
            crate::startup::private_directory(&home)?;
        }
        let mut cache = BTreeMap::new();
        cache.insert(SYSTEM_CODEX.into(), CachedProfile::default());
        for profile in &manifest.profiles {
            cache.insert(profile.id.clone(), CachedProfile::default());
        }
        let manager = Self {
            inner: Arc::new(Inner {
                data_dir,
                homes,
                state: Mutex::new(ProfileState {
                    manifest,
                    disk_bytes,
                    cache,
                }),
                runtime: AccountRuntime::new(),
                reads: Arc::new(Semaphore::new(2)),
                stopped: CancellationToken::new(),
                tasks: Mutex::new(Vec::new()),
            }),
        };
        {
            let mut state = lock(&manager.inner.state);
            if state.disk_bytes.is_none() {
                let manifest = state.manifest.clone();
                manager.persist(&mut state, manifest)?;
            }
        }
        Ok(manager)
    }

    fn ensure_running(&self) -> Result<()> {
        if self.inner.stopped.is_cancelled() {
            Err(AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "accounts_stopping",
                "The account manager is shutting down.",
            ))
        } else {
            Ok(())
        }
    }

    fn record<'a>(state: &'a ProfileState, id: &str) -> Result<Option<&'a ProfileRecord>> {
        if id == SYSTEM_CODEX {
            return Ok(None);
        }
        state
            .manifest
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .map(Some)
            .ok_or_else(|| {
                AppError::new(
                    StatusCode::NOT_FOUND,
                    "profile_not_found",
                    "Account profile not found.",
                )
            })
    }

    fn binding(&self, state: &ProfileState, id: &str) -> Result<ProfileBinding> {
        let Some(profile) = Self::record(state, id)? else {
            return Ok(ProfileBinding {
                id: SYSTEM_CODEX.into(),
                name: "Existing Codex CLI".into(),
                agent_id: "codex".into(),
                home: PathBuf::new(),
                managed: false,
            });
        };
        let home = self.inner.homes.join(&profile.id);
        check_directory_chain(&home)?;
        crate::startup::private_directory(&home)?;
        if home.canonicalize().ok().as_ref() != Some(&home) {
            return Err(AppError::invalid(
                "The account profile directory no longer matches its private location.",
            ));
        }
        Ok(ProfileBinding {
            id: profile.id.clone(),
            name: profile.name.clone(),
            agent_id: profile.agent_id.clone(),
            home,
            managed: true,
        })
    }

    fn view(state: &ProfileState, id: &str) -> Result<ProfileView> {
        let record = Self::record(state, id)?;
        let (name, agent_id, managed) = record
            .map_or(("Existing Codex CLI", "codex", false), |profile| {
                (profile.name.as_str(), profile.agent_id.as_str(), true)
            });
        let cached = state
            .cache
            .get(id)
            .ok_or_else(|| AppError::local("Account cache is unavailable."))?;
        Ok(ProfileView {
            id: id.into(),
            name: name.into(),
            agent_id: agent_id.into(),
            managed,
            read_only: !managed,
            selected: selected(&state.manifest.defaults, agent_id) == Some(id),
            refreshing: cached.refreshing,
            account: cached.account.clone(),
        })
    }

    pub(crate) fn cached(&self) -> (Vec<ProfileView>, Defaults) {
        let state = lock(&self.inner.state);
        let profiles = std::iter::once(SYSTEM_CODEX)
            .chain(
                state
                    .manifest
                    .profiles
                    .iter()
                    .map(|profile| profile.id.as_str()),
            )
            .filter_map(|id| Self::view(&state, id).ok())
            .collect();
        (profiles, state.manifest.defaults.clone())
    }

    pub(crate) fn create_profile(&self, agent_id: &str, name: &str) -> Result<ProfileView> {
        self.ensure_running()?;
        if !valid_agent(agent_id) {
            return Err(AppError::invalid(
                "Choose codex or claude for this account profile.",
            ));
        }
        let name = checked_name(name)?;
        let mut state = lock(&self.inner.state);
        if state.manifest.profiles.len() >= MAX_PROFILES {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "profile_limit",
                "This app already has 32 managed account profiles.",
            ));
        }
        if state
            .manifest
            .profiles
            .iter()
            .any(|profile| profile.agent_id == agent_id && profile.name.eq_ignore_ascii_case(&name))
        {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "profile_name_exists",
                "Choose a different name for this agent's profile.",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let home = self.inner.homes.join(&id);
        if home.exists() {
            return Err(AppError::local(
                "A new account profile directory could not be allocated.",
            ));
        }
        check_directory_chain(&self.inner.homes)?;
        crate::startup::private_directory(&home)?;
        let mut manifest = state.manifest.clone();
        manifest.profiles.push(ProfileRecord {
            id: id.clone(),
            agent_id: agent_id.into(),
            name,
            created_at_ms: now_ms(),
        });
        self.persist(&mut state, manifest)?;
        state.cache.insert(id.clone(), CachedProfile::default());
        Self::view(&state, &id)
    }

    fn persist(&self, state: &mut ProfileState, manifest: Manifest) -> Result<()> {
        validate_manifest(&manifest)?;
        let path = self.inner.data_dir.join("account-profiles.json");
        check_directory_chain(&self.inner.data_dir)?;
        if read_manifest(&path)? != state.disk_bytes {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "profile_metadata_changed",
                "Account metadata changed outside this app. Restart before editing profiles; that file was not overwritten.",
            ));
        }
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| AppError::local("Could not encode account profile metadata."))?;
        write_manifest(&self.inner.data_dir, &path, &bytes)?;
        state.manifest = manifest;
        state.disk_bytes = Some(bytes);
        Ok(())
    }

    fn resolve_locked(
        &self,
        state: &ProfileState,
        agent_id: &str,
        profile_id: Option<&str>,
    ) -> Result<ProfileBinding> {
        if !valid_agent(agent_id) {
            return Err(AppError::invalid(
                "Choose codex or claude for the account profile.",
            ));
        }
        let id = profile_id
            .or_else(|| selected(&state.manifest.defaults, agent_id))
            .ok_or_else(|| AppError::invalid("Select an account profile for this agent first."))?;
        let binding = self.binding(state, id)?;
        if binding.agent_id != agent_id {
            return Err(AppError::invalid(
                "The account profile belongs to a different agent.",
            ));
        }
        Ok(binding)
    }

    /// Hold the same mutex used to reserve login, through adapter registration.
    pub(crate) fn start_run(
        &self,
        adapters: &AdapterManager,
        mut request: StartRequest,
        profile_id: Option<&str>,
    ) -> Result<AdapterRun> {
        self.ensure_running()?;
        let state = lock(&self.inner.state);
        let retained = adapters
            .list_runs()
            .into_iter()
            .find(|run| run.command_id == request.command_id);
        let retained_profile = retained.as_ref().and_then(|run| run.profile_id.as_deref());
        if let Some(run) = &retained
            && (run.adapter_id != request.adapter_id
                || profile_id.is_some_and(|id| Some(id) != retained_profile))
        {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "conflict",
                "command_id already belongs to a different account or adapter request.",
            ));
        }
        // A retry keeps the original profile even if the future default or a
        // cached sign-in check changed after the original request was accepted.
        if retained.is_some() {
            request.profile = adapters.retained_profile(&request.command_id);
            return adapters.start(request).map_err(AppError::from);
        }
        let binding = match request.continue_run_id.as_deref() {
            // A conversation stays with the account that holds its Codex thread.
            Some(previous) => {
                let original = adapters.run_profile(previous)?.ok_or_else(|| {
                    AppError::new(
                        StatusCode::CONFLICT,
                        "conflict",
                        "This run has no recorded account and cannot be continued.",
                    )
                })?;
                if profile_id.is_some_and(|id| id != original.id) {
                    return Err(AppError::new(
                        StatusCode::CONFLICT,
                        "conflict",
                        "Continue with the account that started this conversation.",
                    ));
                }
                let current = self.binding(&state, &original.id).ok();
                if current.as_ref().is_none_or(|current| {
                    current.home != original.home || current.agent_id != original.agent_id
                }) {
                    return Err(AppError::new(
                        StatusCode::CONFLICT,
                        "profile_changed",
                        "The account profile that started this conversation is no longer available. Start a new conversation.",
                    ));
                }
                original
            }
            None => self.resolve_locked(&state, &request.adapter_id, profile_id)?,
        };
        let cached = state
            .cache
            .get(&binding.id)
            .ok_or_else(|| AppError::local("Account cache is unavailable."))?;
        if cached.login_busy || cached.terminal_busy {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "profile_busy",
                "Finish this account's sign-in or terminal launch before starting a run.",
            ));
        }
        if binding.managed && cached.account.auth_status != "signed_in" {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "profile_not_signed_in",
                "Refresh this profile and complete sign-in before starting a run.",
            ));
        }
        request.profile = Some(binding);
        adapters.start(request).map_err(AppError::from)
    }

    pub(crate) fn select_profile(&self, id: &str) -> Result<(ProfileView, Defaults)> {
        self.ensure_running()?;
        let mut state = lock(&self.inner.state);
        let binding = self.binding(&state, id)?;
        let mut manifest = state.manifest.clone();
        match binding.agent_id.as_str() {
            "codex" => manifest.defaults.codex = Some(id.into()),
            "claude" => manifest.defaults.claude = Some(id.into()),
            _ => return Err(AppError::invalid("Unknown profile agent.")),
        }
        self.persist(&mut state, manifest)?;
        Ok((Self::view(&state, id)?, state.manifest.defaults.clone()))
    }

    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(task);
        let mut tasks = lock(&self.inner.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
    }

    pub(crate) fn refresh_profile(&self, id: &str) -> Result<ProfileView> {
        self.ensure_running()?;
        tokio::runtime::Handle::try_current()
            .map_err(|_| AppError::local("Account refresh requires the running app host."))?;
        let binding = {
            let mut state = lock(&self.inner.state);
            let binding = self.binding(&state, id)?;
            let cached = state
                .cache
                .get_mut(id)
                .ok_or_else(|| AppError::local("Account cache is unavailable."))?;
            if cached.login_busy || cached.terminal_busy {
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    "profile_busy",
                    "Finish or cancel sign-in before refreshing this profile.",
                ));
            }
            if cached.refreshing {
                return Self::view(&state, id);
            }
            cached.refreshing = true;
            binding
        };
        let manager = self.clone();
        self.spawn(async move {
            manager.inspect_background(binding).await;
        });
        Self::view(&lock(&self.inner.state), id)
    }

    async fn inspect_background(&self, binding: ProfileBinding) {
        let permit = tokio::select! {
            _ = self.inner.stopped.cancelled() => return,
            permit = self.inner.reads.clone().acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
        };
        let result = tokio::select! {
            _ = self.inner.stopped.cancelled() => return,
            result = self.inner.runtime.inspect(&binding) => result,
        };
        drop(permit);
        let mut state = lock(&self.inner.state);
        if let Some(cached) = state.cache.get_mut(&binding.id) {
            cached.account = match result {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let mut snapshot = unknown_account(&error.to_string());
                    snapshot.checked_at_ms = now_ms();
                    snapshot.usage.status = "error".into();
                    snapshot
                }
            };
            cached.refreshing = false;
        }
    }

    pub(crate) async fn begin_login(
        &self,
        id: &str,
        adapters: &AdapterManager,
    ) -> Result<LoginJob> {
        self.ensure_running()?;
        let binding = {
            let mut state = lock(&self.inner.state);
            let binding = self.binding(&state, id)?;
            if !binding.managed {
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    "system_profile_read_only",
                    "The existing Codex account is read-only here. Create a managed profile to sign in.",
                ));
            }
            let cached = state
                .cache
                .get_mut(id)
                .ok_or_else(|| AppError::local("Account cache is unavailable."))?;
            if cached.login_busy
                || cached.refreshing
                || cached.terminal_busy
                || adapters.profile_has_active_runs(id)
            {
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    "profile_busy",
                    "Finish this profile's active run, refresh or sign-in first.",
                ));
            }
            if cached.account.auth_status != "signed_out" {
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    "profile_sign_in_not_allowed",
                    "Sign-in is available only after a refresh confirms this profile is signed out. Create another profile for another account.",
                ));
            }
            cached.login_busy = true;
            binding
        };
        // AccountRuntime registers and returns synchronously on its first poll.
        let job = match self.inner.runtime.start_login(binding.clone()).await {
            Ok(job) => job,
            Err(error) => {
                if let Some(cached) = lock(&self.inner.state).cache.get_mut(id) {
                    cached.login_busy = false;
                }
                return Err(error.into());
            }
        };
        if let Some(cached) = lock(&self.inner.state).cache.get_mut(id) {
            cached.login = Some(job.clone());
        }
        let manager = self.clone();
        let job_id = job.id.clone();
        self.spawn(async move {
            while let Ok(job) = manager.inner.runtime.login(&job_id) {
                let active = active_login(&job);
                {
                    let mut state = lock(&manager.inner.state);
                    if let Some(cached) = state.cache.get_mut(&binding.id) {
                        cached.login = Some(job);
                        if !active {
                            cached.login_busy = false;
                            cached.refreshing = true;
                        }
                    }
                }
                if !active {
                    // A cancelled browser flow can still have completed at the
                    // provider. Read back actual CLI state after every outcome.
                    manager.inspect_background(binding).await;
                    break;
                }
                tokio::select! {
                    _ = manager.inner.stopped.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
            }
        });
        Ok(job)
    }

    pub(crate) fn cached_login(&self, id: &str) -> Result<Option<LoginJob>> {
        let state = lock(&self.inner.state);
        Self::record(&state, id)?;
        Ok(state.cache.get(id).and_then(|cached| cached.login.clone()))
    }

    pub(crate) async fn cancel_profile_login(&self, id: &str) -> Result<LoginJob> {
        let job = self.cached_login(id)?.ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "login_not_found",
                "This profile has no sign-in to cancel.",
            )
        })?;
        let job = self.inner.runtime.cancel_login(&job.id).await?;
        if let Some(cached) = lock(&self.inner.state).cache.get_mut(id) {
            cached.login = Some(job.clone());
        }
        Ok(job)
    }

    pub(crate) async fn open_profile_login(&self, id: &str) -> Result<()> {
        self.ensure_running()?;
        let (agent, job_id) = {
            let state = lock(&self.inner.state);
            let binding = self.binding(&state, id)?;
            let job = state
                .cache
                .get(id)
                .and_then(|cached| cached.login.as_ref())
                .ok_or_else(|| {
                    AppError::new(
                        StatusCode::CONFLICT,
                        "login_not_ready",
                        "Start sign-in before opening its browser page.",
                    )
                })?;
            (binding.agent_id, job.id.clone())
        };
        let job = self.inner.runtime.login(&job_id)?;
        if job.profile_id != id || job.status != "awaiting_user" {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "login_not_ready",
                "This sign-in is no longer waiting for a browser action.",
            ));
        }
        let url = job
            .auth_url
            .as_deref()
            .and_then(|url| official_auth_url(&agent, url))
            .ok_or_else(|| {
                AppError::new(
                    StatusCode::CONFLICT,
                    "login_not_ready",
                    "The official CLI has not provided a supported browser sign-in URL yet.",
                )
            })?;
        open_browser(&url).await
    }

    pub(crate) async fn open_terminal(&self, id: &str, project: &Path) -> Result<()> {
        self.ensure_running()?;
        let binding = {
            let mut state = lock(&self.inner.state);
            let binding = self.binding(&state, id)?;
            let cached = state
                .cache
                .get_mut(id)
                .ok_or_else(|| AppError::local("Account cache is unavailable."))?;
            if !binding.managed || binding.agent_id != "claude" {
                return Err(AppError::invalid(
                    "External terminals are available here for managed Claude profiles.",
                ));
            }
            if cached.login_busy
                || cached.terminal_busy
                || cached.account.auth_status != "signed_in"
            {
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    "profile_not_ready",
                    "Refresh and sign in to this Claude profile before opening its terminal.",
                ));
            }
            cached.terminal_busy = true;
            binding
        };
        let project = project.to_owned();
        let profile_id = id.to_owned();
        let manager = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || launch_profile_terminal(&binding, &project))
                    .await
                    .map_err(|_| {
                        AppError::local("The external terminal launcher stopped unexpectedly.")
                    })
                    .and_then(|result| result.map_err(AppError::from));
            if let Some(cached) = lock(&manager.inner.state).cache.get_mut(&profile_id) {
                cached.terminal_busy = false;
            }
            let _ = send.send(result);
        });
        receive
            .await
            .map_err(|_| AppError::local("The external terminal launch result is unavailable."))?
    }

    pub(crate) async fn shutdown(&self) {
        self.inner.stopped.cancel();
        let mut tasks = std::mem::take(&mut *lock(&self.inner.tasks));
        let drain = async {
            for task in &mut tasks {
                let _ = task.await;
            }
        };
        let (_, completed) = tokio::join!(
            self.inner.runtime.shutdown(),
            tokio::time::timeout(Duration::from_secs(6), drain)
        );
        if completed.is_err() {
            for task in tasks {
                task.abort();
            }
        }
    }
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != 1 || manifest.profiles.len() > MAX_PROFILES {
        return Err(AppError::invalid(
            "Account profile metadata has an unsupported version or too many profiles.",
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for profile in &manifest.profiles {
        if !managed_id(&profile.id)
            || !valid_agent(&profile.agent_id)
            || checked_name(&profile.name)? != profile.name
            || !ids.insert(profile.id.as_str())
            || !names.insert((profile.agent_id.as_str(), profile.name.to_ascii_lowercase()))
        {
            return Err(AppError::invalid(
                "Account profile metadata contains an invalid or duplicate profile.",
            ));
        }
    }
    for agent in ["codex", "claude"] {
        if let Some(id) = selected(&manifest.defaults, agent) {
            let valid = agent == "codex" && id == SYSTEM_CODEX
                || manifest
                    .profiles
                    .iter()
                    .any(|profile| profile.id == id && profile.agent_id == agent);
            if !valid {
                return Err(AppError::invalid(
                    "An account default refers to an unknown or different agent profile.",
                ));
            }
        }
    }
    Ok(())
}

fn link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn check_directory_chain(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(AppError::invalid("Account directories must be absolute."));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(AppError::invalid(
                "Account paths cannot contain relative navigation.",
            ));
        }
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|_| AppError::invalid("An account directory is missing or inaccessible."))?;
        if !metadata.is_dir() || link_or_reparse(&metadata) {
            return Err(AppError::invalid(
                "Account directories cannot contain symlinks or reparse points.",
            ));
        }
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(AppError::invalid(
                "Could not read private account profile metadata.",
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|_| AppError::invalid("Could not inspect account profile metadata."))?;
    if !metadata.is_file() || link_or_reparse(&metadata) || metadata.len() > MAX_MANIFEST {
        return Err(AppError::invalid(
            "Account metadata must be a small regular file without links.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(AppError::invalid(
                "Account metadata must be owned by this user and private to that user.",
            ));
        }
    }
    #[cfg(windows)]
    crate::launch_acl::verify(&file).map_err(|_| {
        AppError::invalid("Account metadata must have private current-user access.")
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AppError::invalid("Could not read account profile metadata."))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(AppError::invalid("Account profile metadata is too large."));
    }
    Ok(Some(bytes))
}

fn write_manifest(directory: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = directory.join(format!(".account-profiles-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC, WRITE_OWNER};
        options
            .access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
            .share_mode(0);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| AppError::local("Could not create private account profile metadata."))?;
    let result = (|| -> Result<()> {
        #[cfg(windows)]
        crate::launch_acl::restrict(&file).map_err(|_| {
            AppError::local("Could not protect account profile metadata before writing it.")
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| AppError::local("Could not save account profile metadata."))?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(AppError::local(
            "Could not atomically replace account profile metadata.",
        ));
    }
    #[cfg(unix)]
    if let Ok(directory) = File::open(directory) {
        let _ = directory.sync_all();
    }
    Ok(())
}

async fn open_browser(url: &str) -> Result<()> {
    // The URL is already the recorded, allowlisted official login URL. No shell
    // interprets it, and no caller can supply a separate executable or URL here.
    #[cfg(windows)]
    let mut command = {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).filter(|path| path.is_absolute())
            .ok_or_else(|| AppError::local("Windows could not locate its browser launcher. Copy the sign-in URL into your browser."))?;
        let mut command = tokio::process::Command::new(root.join("System32/rundll32.exe"));
        command
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .creation_flags(0x0800_0000);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/bin/open");
        command.arg(url);
        command
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/bin/xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| AppError::local("The system browser could not be opened. Copy the official sign-in URL into your browser."))?;
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => {
            let _ = child.kill().await;
            Err(AppError::local(
                "The browser launch could not be confirmed. Use the official sign-in URL shown in the app.",
            ))
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProfile {
    agent_id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenTerminal {
    project_id: String,
}

pub(crate) async fn list(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>> {
    no_query(request.uri())?;
    let (profiles, defaults) = state.accounts.cached();
    Ok(Json(json!({"profiles":profiles,"defaults":defaults})))
}

pub(crate) async fn create(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: CreateProfile = body(request).await?;
    let profile = state
        .accounts
        .create_profile(&input.agent_id, &input.name)?;
    Ok((StatusCode::CREATED, Json(json!({"profile":profile}))))
}

pub(crate) async fn refresh(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<impl IntoResponse> {
    let _: Empty = body(request).await?;
    let profile = state.accounts.refresh_profile(&id)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"profile":profile}))))
}

pub(crate) async fn start_login(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<impl IntoResponse> {
    let _: Empty = body(request).await?;
    let login = state.accounts.begin_login(&id, &state.adapters).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"login":login}))))
}

pub(crate) async fn login_status(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({"login":state.accounts.cached_login(&id)?})))
}

pub(crate) async fn cancel_login(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _: Empty = body(request).await?;
    Ok(Json(
        json!({"login":state.accounts.cancel_profile_login(&id).await?}),
    ))
}

pub(crate) async fn open_login(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _: Empty = body(request).await?;
    state.accounts.open_profile_login(&id).await?;
    Ok(Json(json!({"opened":true})))
}

pub(crate) async fn select(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _: Empty = body(request).await?;
    let (profile, defaults) = state.accounts.select_profile(&id)?;
    Ok(Json(json!({"profile":profile,"defaults":defaults})))
}

pub(crate) async fn launch_terminal(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: OpenTerminal = body(request).await?;
    let project = state
        .engine
        .list_projects()?
        .into_iter()
        .find(|project| project.id == input.project_id)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "project_not_found",
                "Open the project in this workspace before launching Claude Code.",
            )
        })?;
    state
        .accounts
        .open_terminal(&id, Path::new(&project.root))
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({"launched":true,"message":"Claude Code was requested in an external terminal. This app does not track that session."}),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_persist_only_metadata_and_defaults_without_probing() {
        // No Tokio runtime: opening, creating, selecting and listing must not
        // attempt to spawn any account process or background task.
        let temp = tempfile::tempdir().unwrap();
        let manager = AccountManager::open(temp.path()).unwrap();
        let (profiles, defaults) = manager.cached();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, SYSTEM_CODEX);
        assert!(profiles[0].read_only);
        assert_eq!(defaults.codex.as_deref(), Some(SYSTEM_CODEX));
        let work = manager.create_profile("codex", "  Work  ").unwrap();
        let personal = manager.create_profile("claude", "Personal").unwrap();
        assert_eq!(work.name, "Work");
        assert!(managed_id(&work.id));
        assert_eq!(work.account.auth_status, "unknown");
        assert_eq!(work.account.checked_at_ms, 0);
        assert!(!work.refreshing);
        manager.select_profile(&work.id).unwrap();
        manager.select_profile(&personal.id).unwrap();
        let bytes = read_manifest(&temp.path().join("account-profiles.json"))
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 3);
        for profile in value["profiles"].as_array().unwrap() {
            assert_eq!(profile.as_object().unwrap().len(), 4);
            for forbidden in ["home", "account", "email", "token", "usage", "login"] {
                assert!(profile.get(forbidden).is_none());
            }
        }
        drop(manager);
        let reopened = AccountManager::open(temp.path()).unwrap();
        let (profiles, defaults) = reopened.cached();
        assert_eq!(defaults.codex.as_deref(), Some(work.id.as_str()));
        assert_eq!(defaults.claude.as_deref(), Some(personal.id.as_str()));
        assert!(
            profiles
                .iter()
                .all(|profile| profile.account.auth_status == "unknown" && !profile.refreshing)
        );
        let state = lock(&reopened.inner.state);
        let binding = reopened.resolve_locked(&state, "codex", None).unwrap();
        assert_eq!(binding.id, work.id);
        assert_eq!(binding.home, reopened.inner.homes.join(&work.id));
        assert!(
            reopened
                .resolve_locked(&state, "claude", Some(&work.id))
                .is_err()
        );
    }

    #[test]
    fn metadata_edits_never_overwrite_an_external_change() {
        let temp = tempfile::tempdir().unwrap();
        let manager = AccountManager::open(temp.path()).unwrap();
        let path = temp.path().join("account-profiles.json");
        let externally_changed = b"external edit in progress\n";
        std::fs::write(&path, externally_changed).unwrap();
        let error = match manager.create_profile("codex", "Work") {
            Err(error) => error,
            Ok(_) => panic!("external edit was overwritten"),
        };
        assert_eq!(error.code, "profile_metadata_changed");
        assert_eq!(std::fs::read(&path).unwrap(), externally_changed);
        assert_eq!(manager.cached().0.len(), 1);
        assert!(AccountManager::open(temp.path()).is_err());
    }

    #[test]
    fn profile_inputs_and_persisted_paths_are_bounded() {
        for name in ["", "   ", "new\naccount", "x\0y"] {
            assert!(checked_name(name).is_err());
        }
        assert!(checked_name(&"é".repeat(64)).is_ok());
        assert!(checked_name(&"é".repeat(65)).is_err());
        for value in [
            json!({"agent_id":"codex","name":"Work","home":"/tmp/shared"}),
            json!({"agent_id":"codex","name":"Work","credentials":"secret"}),
            json!({"agent_id":"codex","name":"Work","id":"chosen"}),
        ] {
            assert!(serde_json::from_value::<CreateProfile>(value).is_err());
        }
        let temp = tempfile::tempdir().unwrap();
        let manager = AccountManager::open(temp.path()).unwrap();
        assert!(manager.create_profile("other", "Work").is_err());
        manager.create_profile("codex", "Work").unwrap();
        assert!(manager.create_profile("codex", "work").is_err());
        manager.create_profile("claude", "Work").unwrap();
        let mut manifest = lock(&manager.inner.state).manifest.clone();
        manifest.profiles[0].id = "../../outside".into();
        assert!(validate_manifest(&manifest).is_err());
        for index in 2..MAX_PROFILES {
            manager
                .create_profile("codex", &format!("Profile {index}"))
                .unwrap();
        }
        assert!(manager.create_profile("codex", "Over the limit").is_err());
    }

    #[tokio::test]
    async fn negative_auth_and_run_gates_never_invoke_a_cli() {
        let temp = tempfile::tempdir().unwrap();
        let manager = AccountManager::open(temp.path()).unwrap();
        let profile = manager.create_profile("codex", "Work").unwrap();
        let adapters = AdapterManager::new();
        assert!(manager.begin_login(SYSTEM_CODEX, &adapters).await.is_err());
        assert!(manager.begin_login(&profile.id, &adapters).await.is_err());
        let request = || StartRequest {
            adapter_id: "codex".into(),
            project_path: temp.path().to_owned(),
            prompt: "fixture only".into(),
            command_id: uuid::Uuid::new_v4().to_string(),
            continue_run_id: None,
            profile: None,
        };
        let error = manager
            .start_run(&adapters, request(), Some(&profile.id))
            .unwrap_err();
        assert_eq!(error.code, "profile_not_signed_in");
        {
            let mut state = lock(&manager.inner.state);
            let cached = state.cache.get_mut(&profile.id).unwrap();
            cached.account.auth_status = "signed_in".into();
            cached.login_busy = true;
        }
        let error = manager
            .start_run(&adapters, request(), Some(&profile.id))
            .unwrap_err();
        assert_eq!(error.code, "profile_busy");
        assert!(manager.begin_login(&profile.id, &adapters).await.is_err());
        lock(&manager.inner.state)
            .cache
            .get_mut(&profile.id)
            .unwrap()
            .login_busy = false;
        assert!(manager.begin_login(&profile.id, &adapters).await.is_err());
        assert!(adapters.list_runs().is_empty());
        assert!(manager.cached_login(&profile.id).unwrap().is_none());
        manager.shutdown().await;
        adapters.shutdown().await;
    }

    #[cfg(unix)]
    #[test]
    fn managed_homes_refuse_symlink_replacement_and_keep_private_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let manager = AccountManager::open(temp.path()).unwrap();
        let profile = manager.create_profile("claude", "Work").unwrap();
        let home = manager.inner.homes.join(&profile.id);
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(temp.path().join("account-profiles.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let other = tempfile::tempdir().unwrap();
        std::fs::remove_dir(&home).unwrap();
        symlink(other.path(), &home).unwrap();
        assert!(
            manager
                .binding(&lock(&manager.inner.state), &profile.id)
                .is_err()
        );
        assert!(AccountManager::open(temp.path()).is_err());
        assert!(std::fs::read_dir(other.path()).unwrap().next().is_none());
    }
}
