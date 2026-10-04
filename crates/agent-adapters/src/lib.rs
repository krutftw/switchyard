//! Local, explicitly started Codex app-server runs. Discovery never starts a model.
//!
//! Runs and their bounded event windows are saved by [`AdapterManager::open`],
//! so conversations survive host restarts. Saved work is never replayed: a run
//! that was active when its host stopped is marked for review, and continuing a
//! conversation resumes the saved Codex thread with only the new message.
mod account_process;
mod accounts;
mod discovery;
mod process_tree;
mod profile;
mod protocol;
mod store;
mod terminal;
#[cfg(unix)]
mod terminal_unix;
mod types;

pub use accounts::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
pub use terminal::launch_profile_terminal;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
pub use types::*;

/// Runs retained across restarts. The oldest inactive conversations are removed
/// first when a new run would exceed this bound.
const MAX_RUNS: usize = 200;
const MAX_ACTIVE_RUNS: usize = 4;
const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;

const STOPPED_UNCERTAIN: &str = "Switchya stopped while this run was active. Codex may have finished an approved command or file change. Review the project, then acknowledge recovery before continuing. Nothing was replayed.";
const STOPPED_BEFORE_THREAD: &str =
    "Switchya stopped before Codex created a conversation. No model turn was started.";

#[derive(Clone)]
pub struct AdapterManager {
    inner: Arc<Inner>,
}

struct Inner {
    runs: Mutex<BTreeMap<String, Entry>>,
    /// Command IDs of runs removed by retention. Always locked after `runs`.
    retired: Mutex<HashSet<String>>,
    store: Option<Arc<store::Store>>,
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
    /// Present only for runs started by this host.
    control: Option<mpsc::Sender<Control>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Entry {
    fn process_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|task| task.is_finished())
    }
}

pub(crate) struct StoredRun {
    view: AdapterRun,
    events: VecDeque<AdapterEvent>,
    event_bytes: usize,
    store: Option<Arc<store::Store>>,
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
    /// In-memory manager: runs and Codex threads end with this host.
    pub fn new() -> Self {
        Self::with_store(None, BTreeMap::new(), HashSet::new())
    }

    /// Durable manager backed by `adapter-runs.sqlite3` in `data_dir`. Runs that
    /// were active when the previous host stopped are marked, never resumed.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let store = Arc::new(store::Store::open(data_dir)?);
        let mut runs = BTreeMap::new();
        for loaded in store.load()? {
            let id = loaded.view.id.clone();
            let mut stored = StoredRun {
                view: loaded.view,
                events: VecDeque::new(),
                event_bytes: 0,
                store: Some(store.clone()),
            };
            if stored.view.state.is_active() {
                // A thread may already have received the model turn. Without
                // Codex's confirmation the outcome is unknown.
                let uncertain = stored.view.thread_id.is_some();
                stored.event(
                    "adapter_error",
                    json!({"message": if uncertain { STOPPED_UNCERTAIN } else { STOPPED_BEFORE_THREAD }}),
                );
                stored.state(if uncertain {
                    RunState::RecoveryRequired
                } else {
                    RunState::Failed
                });
                if let Some(error) = stored.view.history_error.take() {
                    return Err(AdapterError::Unavailable(error));
                }
            }
            runs.insert(
                id,
                Entry {
                    request_hash: loaded.request_hash,
                    profile: loaded.profile,
                    run: Arc::new(Mutex::new(stored)),
                    control: None,
                    task: None,
                },
            );
        }
        let retired = store.retired()?.into_iter().collect();
        Ok(Self::with_store(Some(store), runs, retired))
    }

    fn with_store(
        store: Option<Arc<store::Store>>,
        runs: BTreeMap<String, Entry>,
        retired: HashSet<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                runs: Mutex::new(runs),
                retired: Mutex::new(retired),
                store,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub fn is_durable(&self) -> bool {
        self.inner.store.is_some()
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
        project: PathBuf,
        executable: PathBuf,
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
        if self.inner.retired.lock().contains(&request.command_id) {
            return Err(AdapterError::Conflict(
                "This request's run was removed from saved history. It was not started again; send a new message to start new work.".into(),
            ));
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
        let (conversation_id, resume_thread) = match request.continue_run_id.as_deref() {
            None => (id.clone(), None),
            Some(previous) => {
                let (conversation, thread) = continuation(&runs, previous, &request, &project)?;
                (conversation, Some(thread))
            }
        };
        if runs.len() >= MAX_RUNS {
            self.prune_locked(&mut runs, &conversation_id)?;
        }
        let ephemeral = self.inner.store.is_none();
        let view = AdapterRun {
            id: id.clone(),
            adapter_id: request.adapter_id,
            command_id: request.command_id,
            conversation_id,
            continued_from: request.continue_run_id,
            title: task_title(&request.prompt),
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
            ephemeral,
            history_error: None,
            permission_boundary: types::PERMISSION_BOUNDARY.into(),
        };
        let mut stored = StoredRun {
            view,
            events: VecDeque::new(),
            event_bytes: 0,
            store: None,
        };
        stored.event("state_changed", json!({"state": "starting"}));
        stored.event(
            "user_task",
            json!({"text": &request.prompt, "continued": resume_thread.is_some()}),
        );
        if let Some(store) = &self.inner.store {
            // Fail closed: a conversation that cannot be saved is not started.
            store.insert_run(
                &stored.view,
                &request_hash,
                profile.as_ref(),
                stored.events.make_contiguous(),
            )?;
            stored.store = Some(store.clone());
        }
        let state = Arc::new(Mutex::new(stored));
        let (control, receiver) = mpsc::channel(8);
        let task = tokio::spawn(protocol::run(
            executable,
            prefix_args,
            protocol::RunInput {
                project,
                profile: request.profile,
                prompt: request.prompt,
                resume_thread,
                ephemeral,
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
                control: Some(control),
                task: Some(task),
            },
        );
        Ok(result)
    }

    /// Remove the least recently started inactive conversations until one more
    /// run fits. The conversation being continued is never removed.
    fn prune_locked(&self, runs: &mut BTreeMap<String, Entry>, keep: &str) -> Result<()> {
        let mut conversations: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        for entry in runs.values() {
            let run = entry.run.lock();
            let busy = run.view.state.is_active() || !entry.process_finished();
            let slot = conversations
                .entry(run.view.conversation_id.clone())
                .or_insert((0, false));
            slot.0 = slot.0.max(run.view.started_at_ms);
            slot.1 |= busy;
        }
        let mut candidates: Vec<_> = conversations
            .into_iter()
            .filter(|(id, (_, busy))| !busy && id != keep)
            .map(|(id, (latest, _))| (latest, id))
            .collect();
        candidates.sort();
        for (_, conversation) in candidates {
            if runs.len() < MAX_RUNS {
                break;
            }
            if let Some(store) = &self.inner.store {
                store.delete_conversation(&conversation)?;
            }
            let mut removed = Vec::new();
            runs.retain(|_, entry| {
                let run = entry.run.lock();
                let keep = run.view.conversation_id != conversation;
                if !keep {
                    removed.push(run.view.command_id.clone());
                }
                keep
            });
            self.inner.retired.lock().extend(removed);
        }
        if runs.len() >= MAX_RUNS {
            return Err(AdapterError::Limit(format!(
                "This host retains at most {MAX_RUNS} agent runs and none of the older conversations can be removed yet."
            )));
        }
        Ok(())
    }

    /// Newest first.
    pub fn list_runs(&self) -> Vec<AdapterRun> {
        let mut runs: Vec<_> = self
            .inner
            .runs
            .lock()
            .values()
            .map(|e| e.run.lock().view.clone())
            .collect();
        runs.sort_by(|a, b| {
            b.started_at_ms
                .cmp(&a.started_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        runs
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

    /// Trusted host lookup of the account bound to a run, by run id.
    pub fn run_profile(&self, run_id: &str) -> Result<Option<ProfileBinding>> {
        let runs = self.inner.runs.lock();
        let entry = runs
            .get(run_id)
            .ok_or_else(|| AdapterError::NotFound("Adapter run not found.".into()))?;
        Ok(entry.profile.clone())
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
        // The saved window is authoritative, including for runs from earlier
        // hosts. After a save failure, serve this host's in-memory window.
        if let Some(store) = &run.store
            && run.view.history_error.is_none()
        {
            return store.events(id, after_seq, limit);
        }
        Ok(run
            .events
            .iter()
            .filter(|e| e.seq > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    /// Record that the user reviewed a run whose outcome was uncertain. The
    /// outcome remains unknown and nothing is replayed; the conversation may
    /// then be continued explicitly.
    pub fn acknowledge_recovery(&self, id: &str) -> Result<AdapterRun> {
        let runs = self.inner.runs.lock();
        let entry = runs
            .get(id)
            .ok_or_else(|| AdapterError::NotFound("Adapter run not found.".into()))?;
        if !entry.process_finished() {
            return Err(AdapterError::Conflict(
                "The Codex process is still stopping. Try again shortly.".into(),
            ));
        }
        let mut run = entry.run.lock();
        if run.view.state != RunState::RecoveryRequired {
            return Err(AdapterError::Conflict(
                "This run is not waiting for recovery review.".into(),
            ));
        }
        let ended = run.view.ended_at_ms;
        run.event(
            "recovery_acknowledged",
            json!({"message": "You reviewed this interrupted run. Its outcome is still unknown; nothing was replayed."}),
        );
        run.state(RunState::Interrupted);
        run.view.ended_at_ms = ended;
        run.persist(None, &[]);
        Ok(run.view.clone())
    }

    fn control(&self, id: &str) -> Result<mpsc::Sender<Control>> {
        let runs = self.inner.runs.lock();
        let entry = runs
            .get(id)
            .ok_or_else(|| AdapterError::NotFound("Adapter run not found.".into()))?;
        if !entry.run.lock().view.state.is_active() {
            return Err(AdapterError::Conflict("This run has already ended.".into()));
        }
        entry
            .control
            .clone()
            .ok_or_else(|| AdapterError::Conflict("This run has already ended.".into()))
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

/// Validate a continuation under the runs lock. Returns the conversation and the
/// saved Codex thread to resume.
fn continuation(
    runs: &BTreeMap<String, Entry>,
    previous: &str,
    request: &StartRequest,
    project: &Path,
) -> Result<(String, String)> {
    let entry = runs
        .get(previous)
        .ok_or_else(|| AdapterError::NotFound("The run to continue was not found.".into()))?;
    let earlier = entry.run.lock().view.clone();
    if earlier.adapter_id != request.adapter_id {
        return Err(AdapterError::Conflict(
            "Continue this conversation with the agent that started it.".into(),
        ));
    }
    if earlier.state.is_active() || !entry.process_finished() {
        return Err(AdapterError::Conflict(
            "Wait for this run to finish before continuing.".into(),
        ));
    }
    if earlier.state == RunState::RecoveryRequired {
        return Err(AdapterError::Conflict(
            "Review the interrupted run and acknowledge recovery before continuing.".into(),
        ));
    }
    if earlier.ephemeral {
        return Err(AdapterError::Conflict(
            "This run was not saved and cannot be continued. Start a new conversation.".into(),
        ));
    }
    let Some(thread) = earlier.thread_id.clone() else {
        return Err(AdapterError::Conflict(
            "Codex did not create a conversation for this run. Start a new conversation.".into(),
        ));
    };
    if entry.profile != request.profile {
        return Err(AdapterError::Conflict(
            "Continue with the account that started this conversation.".into(),
        ));
    }
    if Path::new(&earlier.project_path) != project {
        return Err(AdapterError::Conflict(
            "Continue in the project folder that started this conversation.".into(),
        ));
    }
    for other in runs.values() {
        let other = other.run.lock();
        if other.view.conversation_id != earlier.conversation_id {
            continue;
        }
        if other.view.state.is_active() {
            return Err(AdapterError::Conflict(
                "Another run in this conversation is still active.".into(),
            ));
        }
        // A continuation that ended before Codex attached to the thread sent
        // nothing to it; it does not supersede the run it tried to continue.
        if other.view.continued_from.as_deref() == Some(previous) && other.view.thread_id.is_some()
        {
            return Err(AdapterError::Conflict(
                "A newer run already continues this conversation. Continue from the latest run."
                    .into(),
            ));
        }
    }
    Ok((earlier.conversation_id, thread))
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
        let mut removed = Vec::new();
        // A completed agent message carries its full text. Drop its streamed
        // fragments so saved history and the bounded window stay compact.
        if kind == "item_completed"
            && event.payload.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage")
            && let Some(item) = event.payload.pointer("/item/id").and_then(Value::as_str)
        {
            let mut bytes = 0usize;
            self.events.retain(|e| {
                let fragment = e.kind == "assistant_delta"
                    && e.payload.get("item_id").and_then(Value::as_str) == Some(item);
                if fragment {
                    removed.push(e.seq);
                    bytes += event_size(e);
                }
                !fragment
            });
            self.event_bytes = self.event_bytes.saturating_sub(bytes);
        }
        self.event_bytes += event_size(&event);
        self.events.push_back(event.clone());
        while self.events.len() > MAX_EVENTS || self.event_bytes > MAX_EVENT_BYTES {
            let Some(oldest) = self.events.pop_front() else {
                break;
            };
            self.event_bytes = self.event_bytes.saturating_sub(event_size(&oldest));
            removed.push(oldest.seq);
            self.view.first_retained_seq = self
                .events
                .front()
                .map_or(self.view.last_seq + 1, |e| e.seq);
        }
        self.persist(Some(&event), &removed);
    }

    pub(crate) fn state(&mut self, state: RunState) {
        self.view.state = state;
        if !state.is_active() {
            self.view.ended_at_ms = Some(types::now_ms());
            self.view.pending_approvals.clear();
        }
        self.event("state_changed", json!({"state": state}));
    }

    /// Save the current view, plus an event and removed rows. After a failure,
    /// this run's remaining history stays in memory and its view reports it.
    pub(crate) fn persist(&mut self, event: Option<&AdapterEvent>, removed: &[u64]) {
        if self.view.history_error.is_some() {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        if store.record(&self.view, event, removed).is_err() {
            self.view.history_error = Some("Switchya could not save this run's history. Its output remains visible until the host exits; after a restart it will require review.".into());
        }
    }
}

fn event_size(event: &AdapterEvent) -> usize {
    serde_json::to_vec(event).map_or(0, |v| v.len())
}

fn task_title(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut title: String = line.chars().filter(|c| !c.is_control()).take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    title
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
        "continue_run_id":request.continue_run_id,
        "profile":request.profile.as_ref().map(|profile| json!({
            "id":profile.id,"name":profile.name,"agent_id":profile.agent_id,
            "home":profile.home,"managed":profile.managed,
        })),
    }))
}

#[cfg(test)]
mod tests;
