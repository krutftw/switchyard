//! Local, explicitly started Codex app-server runs. Discovery never starts a model.
mod account_process;
mod accounts;
mod discovery;
mod process_tree;
mod profile;
mod protocol;
mod terminal;
#[cfg(unix)]
mod terminal_unix;
mod types;

pub use accounts::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
pub use terminal::launch_profile_terminal;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
pub use types::*;

const MAX_RUNS: usize = 32;
const MAX_ACTIVE_RUNS: usize = 4;
const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct AdapterManager {
    inner: Arc<Inner>,
}

struct Inner {
    runs: Mutex<BTreeMap<String, Entry>>,
    shutdown: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct Entry {
    request_hash: String,
    profile: Option<ProfileBinding>,
    run: Arc<Mutex<StoredRun>>,
    control: mpsc::Sender<Control>,
    task: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) struct StoredRun {
    view: AdapterRun,
    events: VecDeque<AdapterEvent>,
    event_bytes: usize,
}

pub(crate) enum Control {
    Decide {
        approval_id: String,
        expected_hash: String,
        decision: ApprovalDecision,
        reply: oneshot::Sender<Result<()>>,
    },
    Interrupt {
        reply: oneshot::Sender<Result<()>>,
    },
}

impl Default for AdapterManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                runs: Mutex::new(BTreeMap::new()),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub async fn discover(&self) -> Vec<AdapterStatus> {
        discovery::discover().await
    }

    pub fn start(&self, request: StartRequest) -> Result<AdapterRun> {
        if let Some(run) = self.retry_existing(&request)? {
            return Ok(run);
        }
        if self.inner.shutdown.is_cancelled() {
            return Err(AdapterError::Unavailable(
                "The adapter host is shutting down.".into(),
            ));
        }
        if request.adapter_id != "codex" {
            return Err(AdapterError::Invalid(
                "Only the Codex adapter can start runs.".into(),
            ));
        }
        if let Some(profile) = &request.profile {
            profile::validate(profile, &request.adapter_id)?;
        }
        if uuid::Uuid::parse_str(&request.command_id).is_err() {
            return Err(AdapterError::Invalid("command_id must be a UUID.".into()));
        }
        if request.prompt.trim().is_empty()
            || request.prompt.len() > 64 * 1024
            || request.prompt.contains('\0')
        {
            return Err(AdapterError::Invalid(
                "Prompt must contain text and be at most 64 KiB without NUL characters.".into(),
            ));
        }
        if !request.project_path.is_absolute() {
            return Err(AdapterError::Invalid(
                "Project path must be absolute.".into(),
            ));
        }
        let project = request
            .project_path
            .canonicalize()
            .map_err(|_| AdapterError::Invalid("Project directory cannot be opened.".into()))?;
        if !project.is_dir() {
            return Err(AdapterError::Invalid(
                "Project path must be a directory.".into(),
            ));
        }
        let executable = discovery::find_cli("codex").ok_or_else(|| AdapterError::Unavailable("Codex native executable was not found. Install the Codex CLI, then refresh discovery.".into()))?;
        self.start_process(request, project, executable, Vec::new())
    }

    fn start_process(
        &self,
        request: StartRequest,
        project: std::path::PathBuf,
        executable: std::path::PathBuf,
        prefix_args: Vec<String>,
    ) -> Result<AdapterRun> {
        let request_hash = original_request_hash(&request);
        let profile = request.profile.clone();
        let mut runs = self.inner.runs.lock();
        for entry in runs.values() {
            let run = entry.run.lock();
            if run.view.command_id == request.command_id {
                return if entry.request_hash == request_hash {
                    Ok(run.view.clone())
                } else {
                    Err(AdapterError::Conflict(
                        "command_id already belongs to a different adapter request.".into(),
                    ))
                };
            }
        }
        if runs.len() >= MAX_RUNS {
            return Err(AdapterError::Limit("This host retained 32 adapter runs. Restart the host to start more; existing runs will not be replayed.".into()));
        }
        if runs
            .values()
            .filter(|e| e.run.lock().view.state.is_active())
            .count()
            >= MAX_ACTIVE_RUNS
        {
            return Err(AdapterError::Limit(
                "At most four external adapter runs may be active.".into(),
            ));
        }
        let id = format!("adapter_{}", uuid::Uuid::new_v4().simple());
        let view = AdapterRun {
            id: id.clone(),
            adapter_id: request.adapter_id,
            command_id: request.command_id,
            profile_id: request.profile.as_ref().map(|p| p.id.clone()),
            profile_name: request.profile.as_ref().map(|p| p.name.clone()),
            project_path: project.to_string_lossy().into_owned(),
            state: RunState::Starting,
            thread_id: None,
            turn_id: None,
            model: None,
            started_at_ms: types::now_ms(),
            ended_at_ms: None,
            last_seq: 0,
            first_retained_seq: 1,
            pending_approvals: Vec::new(),
            ephemeral: true,
            permission_boundary: types::PERMISSION_BOUNDARY.into(),
        };
        let state = Arc::new(Mutex::new(StoredRun {
            view,
            events: VecDeque::new(),
            event_bytes: 0,
        }));
        state
            .lock()
            .event("state_changed", json!({"state": "starting"}));
        state
            .lock()
            .event("user_task", json!({"text": &request.prompt}));
        let (control, receiver) = mpsc::channel(8);
        let task = tokio::spawn(protocol::run(
            executable,
            prefix_args,
            protocol::RunInput {
                project,
                profile: request.profile,
                prompt: request.prompt,
            },
            state.clone(),
            receiver,
            self.inner.shutdown.child_token(),
        ));
        let result = state.lock().view.clone();
        runs.insert(
            id,
            Entry {
                request_hash,
                profile,
                run: state,
                control,
                task: Some(task),
            },
        );
        Ok(result)
    }

    pub fn list_runs(&self) -> Vec<AdapterRun> {
        self.inner
            .runs
            .lock()
            .values()
            .map(|e| e.run.lock().view.clone())
            .collect()
    }

    /// Pure lookup for a previously accepted complete request. It does not check
    /// current CLI installation, credentials, profile directories or project files.
    pub fn retry_existing(&self, request: &StartRequest) -> Result<Option<AdapterRun>> {
        let runs = self.inner.runs.lock();
        for entry in runs.values() {
            let run = entry.run.lock();
            if run.view.command_id == request.command_id {
                return if entry.request_hash == original_request_hash(request) {
                    Ok(Some(run.view.clone()))
                } else {
                    Err(AdapterError::Conflict(
                        "command_id already belongs to a different adapter request.".into(),
                    ))
                };
            }
        }
        Ok(None)
    }

    /// Trusted host lookup only. Never serialize this credential-directory binding
    /// into an HTTP response. Defaults cannot change a retained run's selection.
    pub fn retained_profile(&self, command_id: &str) -> Option<ProfileBinding> {
        self.inner.runs.lock().values().find_map(|entry| {
            (entry.run.lock().view.command_id == command_id)
                .then(|| entry.profile.clone())
                .flatten()
        })
    }

    pub fn profile_has_active_runs(&self, profile_id: &str) -> bool {
        self.inner.runs.lock().values().any(|entry| {
            let run = entry.run.lock();
            run.view.profile_id.as_deref() == Some(profile_id) && run.view.state.is_active()
        })
    }

    pub fn run(&self, id: &str) -> Result<AdapterRun> {
        let runs = self.inner.runs.lock();
        let entry = runs
            .get(id)
            .ok_or_else(|| AdapterError::NotFound("Adapter run not found.".into()))?;
        Ok(entry.run.lock().view.clone())
    }

    pub fn events(&self, id: &str, after_seq: u64, limit: usize) -> Result<Vec<AdapterEvent>> {
        if !(1..=200).contains(&limit) {
            return Err(AdapterError::Invalid(
                "Event limit must be from 1 to 200.".into(),
            ));
        }
        let runs = self.inner.runs.lock();
        let entry = runs
            .get(id)
            .ok_or_else(|| AdapterError::NotFound("Adapter run not found.".into()))?;
        let run = entry.run.lock();
        if after_seq > run.view.last_seq {
            return Err(AdapterError::Invalid(
                "Event cursor is ahead of the run.".into(),
            ));
        }
        Ok(run
            .events
            .iter()
            .filter(|e| e.seq > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    fn control(&self, id: &str) -> Result<mpsc::Sender<Control>> {
        let runs = self.inner.runs.lock();
        let entry = runs
            .get(id)
            .ok_or_else(|| AdapterError::NotFound("Adapter run not found.".into()))?;
        if !entry.run.lock().view.state.is_active() {
            return Err(AdapterError::Conflict("This run has already ended.".into()));
        }
        Ok(entry.control.clone())
    }

    pub async fn decide(
        &self,
        id: &str,
        approval_id: &str,
        expected_hash: &str,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let control = self.control(id)?;
        let (reply, receive) = oneshot::channel();
        control
            .try_send(Control::Decide {
                approval_id: approval_id.into(),
                expected_hash: expected_hash.into(),
                decision,
                reply,
            })
            .map_err(|_| {
                AdapterError::Conflict("Adapter control is busy or unavailable.".into())
            })?;
        receive.await.map_err(|_| {
            AdapterError::Conflict(
                "The adapter stopped before the decision could be confirmed; do not replay it."
                    .into(),
            )
        })?
    }

    pub async fn interrupt(&self, id: &str) -> Result<()> {
        let control = self.control(id)?;
        let (reply, receive) = oneshot::channel();
        control
            .try_send(Control::Interrupt { reply })
            .map_err(|_| {
                AdapterError::Conflict("Adapter control is busy or unavailable.".into())
            })?;
        receive.await.map_err(|_| {
            AdapterError::Conflict("The adapter stopped while interruption was requested.".into())
        })?
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let tasks: Vec<_> = self
            .inner
            .runs
            .lock()
            .values_mut()
            .filter_map(|entry| entry.task.take())
            .collect();
        for task in tasks {
            let _ = task.await;
        }
    }
}

impl StoredRun {
    pub(crate) fn event(&mut self, kind: &str, payload: Value) {
        self.view.last_seq += 1;
        let event = AdapterEvent {
            schema_version: 1,
            run_id: self.view.id.clone(),
            seq: self.view.last_seq,
            at_ms: types::now_ms(),
            kind: kind.into(),
            payload,
        };
        let size = serde_json::to_vec(&event).map_or(0, |v| v.len());
        self.event_bytes += size;
        self.events.push_back(event);
        while self.events.len() > MAX_EVENTS || self.event_bytes > MAX_EVENT_BYTES {
            if let Some(removed) = self.events.pop_front() {
                self.event_bytes = self
                    .event_bytes
                    .saturating_sub(serde_json::to_vec(&removed).map_or(0, |v| v.len()));
            }
        }
        self.view.first_retained_seq = self
            .events
            .front()
            .map_or(self.view.last_seq + 1, |e| e.seq);
    }

    pub(crate) fn state(&mut self, state: RunState) {
        self.view.state = state;
        if !state.is_active() {
            self.view.ended_at_ms = Some(types::now_ms());
            self.view.pending_approvals.clear();
        }
        self.event("state_changed", json!({"state": state}));
    }
}

pub(crate) fn hash_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("JSON values always serialize");
    format!("{:x}", Sha256::digest(bytes))
}

fn original_request_hash(request: &StartRequest) -> String {
    // Use the accepted input, not a fresh filesystem resolution. In particular,
    // deleting or moving a project after acceptance must not invalidate a retry.
    hash_value(&json!({
        "adapter_id":request.adapter_id,
        "project_path":request.project_path,
        "prompt":request.prompt,
        "command_id":request.command_id,
        "profile":request.profile.as_ref().map(|profile| json!({
            "id":profile.id,"name":profile.name,"agent_id":profile.agent_id,
            "home":profile.home,"managed":profile.managed,
        })),
    }))
}

#[cfg(test)]
mod tests;
