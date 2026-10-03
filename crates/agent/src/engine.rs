use crate::gateway;
use crate::store::{self, Store};
use crate::types::*;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use switchyard_agent_tools::{PreparedTool, ToolResult};
use switchyard_core::{FinishReason, ToolCallKind};
use switchyard_gateway::{ClientIdentity, Gateway};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct Pending {
    operation: Operation,
    sender: oneshot::Sender<Decision>,
}

struct ActiveRun {
    id: String,
    cancel: CancellationToken,
    pending: Option<Pending>,
    task: Option<JoinHandle<()>>,
}

struct Inner {
    gateway: Gateway,
    identity: ClientIdentity,
    store: Store,
    options: EngineOptions,
    active: Mutex<HashMap<String, ActiveRun>>,
    /// Serialize approved writes/commands across the host, including projects
    /// with overlapping roots. This trades mutation throughput for reliable
    /// stale-hash checks; model requests, reads and approval waits stay concurrent.
    mutations: tokio::sync::Mutex<()>,
    closing: AtomicBool,
}

/// Shared engine handle. Mutations are journalled before local side effects.
#[derive(Clone)]
pub struct AppEngine {
    inner: Arc<Inner>,
}

impl AppEngine {
    pub fn open(
        gateway: Gateway,
        identity: ClientIdentity,
        options: EngineOptions,
    ) -> Result<Self> {
        if identity.internal || identity.anonymous || identity.key_id.is_none() {
            return Err(AgentError::Permission(
                "the app requires an authenticated gateway client key; admin and anonymous identities cannot run agents".into(),
            ));
        }
        gateway
            .refresh_identity(&identity)
            .map_err(|e| AgentError::Permission(e.message))?;
        if options.max_steps == 0
            || options.max_steps > 100
            || options.max_output_tokens == 0
            || options.max_output_tokens > 65_536
            || options.max_context_bytes < 4096
            || options.max_context_bytes > 32 * 1024 * 1024
        {
            return Err(AgentError::Invalid(
                "invalid agent step, token or context limit".into(),
            ));
        }
        let store = Store::open(&options.data_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                gateway,
                identity,
                store,
                options,
                active: Mutex::new(HashMap::new()),
                mutations: tokio::sync::Mutex::new(()),
                closing: AtomicBool::new(false),
            }),
        })
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            active_runs: self.inner.active.lock().len(),
            permission_mode: "review_writes_and_commands".into(),
            shell_is_sandboxed: false,
            max_steps: self.inner.options.max_steps,
            max_output_tokens: self.inner.options.max_output_tokens,
            max_context_bytes: self.inner.options.max_context_bytes,
        }
    }

    pub fn models(&self) -> Result<Vec<Value>> {
        let identity = self
            .inner
            .gateway
            .refresh_identity(&self.inner.identity)
            .map_err(|e| AgentError::Permission(e.message))?;
        Ok(self
            .inner
            .gateway
            .models(switchyard_core::Protocol::OpenaiResponses, &identity)
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        self.inner.store.projects()
    }

    pub fn open_project(&self, path: impl AsRef<Path>) -> Result<Project> {
        let root = std::fs::canonicalize(path.as_ref()).map_err(|_| {
            AgentError::Invalid("project path does not name an accessible directory".into())
        })?;
        if !root.is_dir() {
            return Err(AgentError::Invalid(
                "project path must be a directory".into(),
            ));
        }
        let root = root
            .to_str()
            .ok_or_else(|| AgentError::Invalid("project path is not valid Unicode".into()))?
            .to_owned();
        self.inner.store.transaction(|tx| {
            if let Some(existing) = tx
                .query_row("SELECT value FROM projects WHERE root=?1", [&root], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
            {
                return Ok(serde_json::from_str(&existing)?);
            }
            let project = Project {
                id: new_id("project"),
                name: Path::new(&root)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Project")
                    .to_owned(),
                root,
                created_at_ms: now_ms(),
            };
            tx.execute(
                "INSERT INTO projects(id,root,value) VALUES(?1,?2,?3)",
                params![project.id, project.root, serde_json::to_string(&project)?],
            )?;
            Ok(project)
        })
    }

    pub fn list_sessions(&self, project_id: Option<&str>) -> Result<Vec<Session>> {
        if let Some(id) = project_id {
            self.inner.store.project(id)?;
        }
        self.inner.store.sessions(project_id)
    }

    pub fn create_session(&self, project_id: &str, model: &str) -> Result<Session> {
        let model = model.trim();
        if model.is_empty()
            || !self
                .models()?
                .iter()
                .any(|entry| entry.get("id").and_then(Value::as_str) == Some(model))
        {
            return Err(AgentError::Invalid(
                "choose a model currently available to the app's client key".into(),
            ));
        }
        self.inner.store.transaction(|tx| {
            store::get_project(tx, project_id)?;
            let now = now_ms();
            let session = Session {
                id: new_id("session"),
                project_id: project_id.into(),
                model: model.into(),
                title: "New session".into(),
                state: SessionState::Idle,
                active_run_id: None,
                revision: 0,
                last_seq: 0,
                created_at_ms: now,
                updated_at_ms: now,
            };
            tx.execute(
                "INSERT INTO sessions(id,project_id,value) VALUES(?1,?2,?3)",
                params![
                    session.id,
                    session.project_id,
                    serde_json::to_string(&session)?
                ],
            )?;
            Ok(session)
        })
    }

    pub fn session(&self, id: &str) -> Result<SessionView> {
        let session = self.inner.store.session(id)?;
        let project = self.inner.store.project(&session.project_id)?;
        let pending_operation = self.inner.store.pending(id)?;
        let events = self
            .inner
            .store
            .events(id, session.last_seq.saturating_sub(200), 200)?;
        Ok(SessionView {
            session,
            project,
            pending_operation,
            events,
        })
    }

    pub fn events(&self, session_id: &str, after_seq: u64, limit: usize) -> Result<Vec<Event>> {
        if limit == 0 || limit > 500 {
            return Err(AgentError::Invalid(
                "event limit must be between 1 and 500".into(),
            ));
        }
        self.inner.store.events(session_id, after_seq, limit)
    }

    /// Record the human's review of an uncertain outcome. This permits a new
    /// turn; it neither restarts work nor changes the old operation into success.
    pub fn acknowledge_recovery(
        &self,
        session_id: &str,
        expected_revision: u64,
        note: &str,
    ) -> Result<Session> {
        let note = note.trim();
        if note.is_empty() || note.len() > 2000 {
            return Err(AgentError::Invalid(
                "a recovery review note of 1-2000 bytes is required".into(),
            ));
        }
        let active = self.inner.active.lock();
        if self.inner.closing.load(Ordering::Acquire) || active.contains_key(session_id) {
            return Err(AgentError::Conflict("the session cannot acknowledge recovery while a run is active or the app is stopping".into()));
        }
        self.inner
            .store
            .transaction(|tx| store::acknowledge_recovery(tx, session_id, expected_revision, note))
    }

    /// A stable command ID makes retries safe. A new command cannot overlap a run.
    pub fn submit_turn(&self, session_id: &str, command_id: &str, text: &str) -> Result<Run> {
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(AgentError::Conflict("the app is shutting down".into()));
        }
        if text.trim().is_empty() || text.len() > 128 * 1024 {
            return Err(AgentError::Invalid(
                "turn text must be nonempty and at most 128 KiB".into(),
            ));
        }
        if command_id.is_empty()
            || command_id.len() > 128
            || !command_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(AgentError::Invalid(
                "command_id must be 1-128 letters, digits, hyphens or underscores".into(),
            ));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            AgentError::Invalid("submit_turn must run inside the app's async runtime".into())
        })?;
        let fingerprint = format!("{:x}", Sha256::digest(text.as_bytes()));
        let mut active = self.inner.active.lock();
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(AgentError::Conflict("the app is shutting down".into()));
        }
        let (run, created) = self.inner.store.transaction(|tx| {
            let mut session = store::get_session(tx, session_id)?;
            if let Some(existing) = store::run_by_command(tx, session_id, command_id, &fingerprint)? {
                return Ok((existing, false));
            }
            if active.contains_key(session_id) || matches!(session.state, SessionState::Running | SessionState::AwaitingApproval) {
                return Err(AgentError::Conflict("this session already has an active run".into()));
            }
            if session.state == SessionState::RecoveryRequired {
                return Err(AgentError::Conflict("inspect the interrupted operation's effects and acknowledge recovery before continuing; uncertain operations are not replayed".into()));
            }
            let project = store::get_project(tx, &session.project_id)?;
            if !Path::new(&project.root).is_dir() {
                return Err(AgentError::Invalid("the bound project directory is no longer available".into()));
            }
            let run = Run { id: new_id("run"), session_id: session_id.into(), command_id: command_id.into(), state: SessionState::Running, started_at_ms: now_ms(), ended_at_ms: None };
            tx.execute("INSERT INTO runs(id,session_id,command_id,fingerprint,value) VALUES(?1,?2,?3,?4,?5)", params![run.id,session_id,command_id,fingerprint,serde_json::to_string(&run)?])?;
            session.state = SessionState::Running;
            session.active_run_id = Some(run.id.clone());
            if session.title == "New session" {
                session.title = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
            }
            store::save_session(tx, &session)?;
            store::append_transcript(tx, session_id, &[json!({"role":"user","content":text})])?;
            store::append_event(tx, session_id, Some(&run.id), "turn.started", json!({"text":text,"command_id":command_id}))?;
            Ok((run, true))
        })?;
        if !created {
            return Ok(run);
        }
        let cancel = CancellationToken::new();
        active.insert(
            session_id.into(),
            ActiveRun {
                id: run.id.clone(),
                cancel: cancel.clone(),
                pending: None,
                task: None,
            },
        );
        let engine = self.clone();
        let task_run = run.clone();
        let task = runtime.spawn(async move {
            let outcome = engine.drive(&task_run, cancel).await;
            // A failed journal write keeps the durable run active so next
            // startup performs recovery; it must never be reported completed.
            let _ = engine.settle(&task_run, outcome);
            engine.inner.active.lock().remove(&task_run.session_id);
        });
        if let Some(entry) = active.get_mut(session_id) {
            entry.task = Some(task);
        }
        Ok(run)
    }

    pub fn decide_operation(
        &self,
        session_id: &str,
        operation_id: &str,
        expected_hash: &str,
        decision: Decision,
    ) -> Result<()> {
        let mut active = self.inner.active.lock();
        let entry = active.get_mut(session_id).ok_or_else(|| {
            AgentError::Conflict("the operation is no longer awaiting approval".into())
        })?;
        if entry.cancel.is_cancelled() {
            return Err(AgentError::Conflict(
                "the run has been interrupted; its approvals have expired".into(),
            ));
        }
        let pending = entry.pending.as_ref().ok_or_else(|| {
            AgentError::Conflict("the operation is no longer awaiting approval".into())
        })?;
        if pending.operation.id != operation_id || pending.operation.arguments_hash != expected_hash
        {
            return Err(AgentError::Conflict(
                "the approval does not match the current operation and exact argument hash".into(),
            ));
        }
        let mut operation = pending.operation.clone();
        operation.state = if decision == Decision::AllowOnce {
            "approved"
        } else {
            "denied"
        }
        .into();
        self.inner.store.transaction(|tx| {
            store::save_operation(tx, &operation)?;
            let mut session = store::get_session(tx, session_id)?;
            session.state = SessionState::Running;
            store::save_session(tx, &session)?;
            store::append_event(
                tx,
                session_id,
                Some(&entry.id),
                "approval.decided",
                json!({"operation_id":operation_id,"decision":decision}),
            )?;
            Ok(())
        })?;
        let pending = entry
            .pending
            .take()
            .ok_or_else(|| AgentError::Conflict("approval expired".into()))?;
        pending.sender.send(decision).map_err(|_| {
            AgentError::Conflict("the run stopped before receiving the decision".into())
        })
    }

    pub fn interrupt(&self, session_id: &str, run_id: &str) -> Result<()> {
        let active = self.inner.active.lock();
        let entry = active
            .get(session_id)
            .ok_or_else(|| AgentError::Conflict("this session has no active run".into()))?;
        if entry.id != run_id {
            return Err(AgentError::Conflict(
                "run_id does not match this session's active run".into(),
            ));
        }
        entry.cancel.cancel();
        self.inner.store.event(session_id, run_id, "run.interrupt_requested", json!({"message":"Stopping the model request or active tool. Completed side effects are not rolled back."}))?;
        Ok(())
    }

    /// Stop active model calls/processes and flush their final journal entries.
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.closing.store(true, Ordering::Release);
        let tasks: Vec<_> = {
            let mut active = self.inner.active.lock();
            active
                .values_mut()
                .filter_map(|entry| {
                    entry.cancel.cancel();
                    entry.task.take()
                })
                .collect()
        };
        let mut incomplete = false;
        for mut task in tasks {
            match tokio::time::timeout(std::time::Duration::from_secs(10), &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => incomplete = true,
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    incomplete = true;
                }
            }
        }
        if incomplete {
            Err(AgentError::Conflict("a run could not drain; its persisted operations will require recovery on next startup".into()))
        } else {
            Ok(())
        }
    }

    async fn drive(&self, run: &Run, cancel: CancellationToken) -> Result<u32> {
        let session = self.inner.store.session(&run.session_id)?;
        let project = self.inner.store.project(&session.project_id)?;
        for step in 1..=self.inner.options.max_steps {
            if cancel.is_cancelled() {
                return Err(AgentError::Interrupted);
            }
            let transcript = self.inner.store.transcript(&session.id)?;
            let mut buffered_text = String::new();
            let mut flushed = std::time::Instant::now();
            let reply = gateway::generate(
                &self.inner.gateway,
                &self.inner.identity,
                &session.id,
                &session.model,
                &project.root,
                transcript,
                self.inner.options.max_output_tokens,
                self.inner.options.max_context_bytes,
                cancel.clone(),
                |text| {
                    buffered_text.push_str(text);
                    if buffered_text.len() >= 128
                        || flushed.elapsed() >= std::time::Duration::from_millis(100)
                    {
                        self.inner.store.event(
                            &session.id,
                            &run.id,
                            "model.delta",
                            json!({"text":std::mem::take(&mut buffered_text)}),
                        )?;
                        flushed = std::time::Instant::now();
                    }
                    Ok(())
                },
            )
            .await;
            if !buffered_text.is_empty() {
                self.inner.store.event(
                    &session.id,
                    &run.id,
                    "model.delta",
                    json!({"text":buffered_text}),
                )?;
            }
            let reply = reply?;
            if cancel.is_cancelled() {
                return Err(AgentError::Interrupted);
            }
            // Calls are inspected only after the complete model response and
            // all signatures have been decoded and durably recorded.
            self.inner.store.transaction(|tx| {
                store::append_transcript(tx, &session.id, &reply.output)?;
                store::append_event(tx, &session.id, Some(&run.id), "model.completed", json!({"text":reply.response.text(),"finish":reply.response.finish,"usage":reply.response.usage,"request_id":reply.request_id,"step":step}))?;
                Ok(())
            })?;
            let calls: Vec<_> = reply.response.tool_calls().cloned().collect();
            if calls.is_empty() {
                if matches!(
                    reply.response.finish,
                    FinishReason::ToolCalls | FinishReason::PauseTurn
                ) {
                    return Err(AgentError::Gateway(
                        "the model requested continuation without an executable tool call".into(),
                    ));
                }
                return Ok(step);
            }
            if calls.len() > 16 {
                return Err(AgentError::Limit(
                    "a model response requested more than 16 tools".into(),
                ));
            }
            let mut ids = std::collections::HashSet::new();
            for call in &calls {
                if call.id.is_empty() || !ids.insert(call.id.clone()) {
                    return Err(AgentError::Gateway(
                        "model tool calls did not have unique pairing IDs".into(),
                    ));
                }
            }
            for call in calls {
                if cancel.is_cancelled() {
                    return Err(AgentError::Interrupted);
                }
                if call.kind != ToolCallKind::Function {
                    self.invalid_tool(
                        run,
                        &call.id,
                        &call.name,
                        "only declared JSON function tools are supported",
                    )?;
                    continue;
                }
                let args: Value = match serde_json::from_str(&call.arguments) {
                    Ok(value @ Value::Object(_)) => value,
                    _ => {
                        self.invalid_tool(
                            run,
                            &call.id,
                            &call.name,
                            "tool arguments must be a complete JSON object",
                        )?;
                        continue;
                    }
                };
                let prepared = match switchyard_agent_tools::prepare(
                    Path::new(&project.root),
                    &call.name,
                    &args,
                ) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.invalid_tool(run, &call.id, &call.name, &error.to_string())?;
                        continue;
                    }
                };
                self.execute_tool(run, &call.id, prepared, cancel.clone())
                    .await?;
            }
        }
        Err(AgentError::Limit(format!(
            "the run reached its {} model-step limit; submit a new turn to continue",
            self.inner.options.max_steps
        )))
    }

    fn invalid_tool(&self, run: &Run, call_id: &str, name: &str, error: &str) -> Result<()> {
        let output = json!({"status":"failed","error":error});
        self.inner.store.transaction(|tx| {
            store::append_transcript(tx, &run.session_id, &[json!({"type":"function_call_output","call_id":call_id,"output":serde_json::to_string(&output)?})])?;
            store::append_event(tx, &run.session_id, Some(&run.id), "tool.rejected", json!({"call_id":call_id,"name":name,"error":error}))?;
            Ok(())
        })
    }

    async fn execute_tool(
        &self,
        run: &Run,
        call_id: &str,
        prepared: PreparedTool,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut operation = Operation {
            id: new_id("operation"),
            session_id: run.session_id.clone(),
            run_id: run.id.clone(),
            call_id: call_id.into(),
            name: prepared.name.clone(),
            arguments_hash: prepared.arguments_hash.clone(),
            arguments: prepared.args.clone(),
            requires_approval: prepared.requires_approval,
            state: "proposed".into(),
            preview: prepared.preview.clone(),
        };
        self.inner.store.transaction(|tx| {
            store::save_operation(tx, &operation)?;
            store::append_event(
                tx,
                &run.session_id,
                Some(&run.id),
                "tool.proposed",
                serde_json::to_value(&operation)?,
            )?;
            Ok(())
        })?;
        if operation.requires_approval {
            let receiver = {
                let mut active = self.inner.active.lock();
                let entry = active
                    .get_mut(&run.session_id)
                    .ok_or(AgentError::Interrupted)?;
                if entry.cancel.is_cancelled() {
                    return Err(AgentError::Interrupted);
                }
                let (sender, receiver) = oneshot::channel();
                operation.state = "awaiting_approval".into();
                self.inner.store.transaction(|tx| {
                    let mut session = store::get_session(tx, &run.session_id)?;
                    session.state = SessionState::AwaitingApproval;
                    store::save_session(tx, &session)?;
                    store::save_operation(tx, &operation)?;
                    store::append_event(
                        tx,
                        &run.session_id,
                        Some(&run.id),
                        "approval.required",
                        serde_json::to_value(&operation)?,
                    )?;
                    Ok(())
                })?;
                entry.pending = Some(Pending {
                    operation: operation.clone(),
                    sender,
                });
                receiver
            };
            let decision = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::Interrupted),
                decision = receiver => decision.map_err(|_| AgentError::Interrupted)?,
            };
            if decision == Decision::Deny {
                operation.state = "denied".into();
                self.record_result(run, &operation, json!({"status":"denied","error":"The user denied this operation. Do not disguise it or attempt to bypass this decision."}))?;
                return Ok(());
            }
        }
        if cancel.is_cancelled() {
            return Err(AgentError::Interrupted);
        }
        let _mutation_guard = if operation.requires_approval {
            Some(tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::Interrupted),
                guard = self.inner.mutations.lock() => guard,
            })
        } else {
            None
        };
        operation.state = "started".into();
        self.inner.store.transaction(|tx| {
            store::save_operation(tx, &operation)?;
            store::append_event(
                tx,
                &run.session_id,
                Some(&run.id),
                "tool.started",
                json!({"operation_id":operation.id,"name":operation.name}),
            )?;
            Ok(())
        })?;
        let result: ToolResult = switchyard_agent_tools::execute(&prepared, cancel.clone()).await;
        operation.state = result.status.clone();
        self.record_result(run, &operation, serde_json::to_value(&result)?)?;
        if cancel.is_cancelled() {
            return Err(AgentError::Interrupted);
        }
        Ok(())
    }

    fn record_result(&self, run: &Run, operation: &Operation, result: Value) -> Result<()> {
        self.inner.store.transaction(|tx| {
            store::save_operation(tx, operation)?;
            store::append_transcript(tx, &run.session_id, &[json!({"type":"function_call_output","call_id":operation.call_id,"output":serde_json::to_string(&result)?})])?;
            store::append_event(tx, &run.session_id, Some(&run.id), "tool.completed", json!({"operation_id":operation.id,"name":operation.name,"result":result}))?;
            Ok(())
        })
    }

    fn settle(&self, run: &Run, outcome: Result<u32>) -> Result<()> {
        self.inner.store.transaction(|tx| {
            let mut query = tx.prepare("SELECT value FROM operations WHERE run_id=?1 AND state IN ('proposed','awaiting_approval','approved','started')")?;
            let operations = query.query_map([&run.id], |r| r.get::<_,String>(0))?
                .map(|r| Ok(serde_json::from_str::<Operation>(&r?)?)).collect::<Result<Vec<_>>>()?;
            let mut unknown = false;
            for mut operation in operations {
                let uncertain = operation.state == "started" && operation.requires_approval;
                unknown |= uncertain;
                operation.state = if uncertain { "outcome_unknown" } else { "expired" }.into();
                store::save_operation(tx, &operation)?;
            }
            let (state, kind, payload) = if unknown {
                (SessionState::RecoveryRequired, "recovery.required", json!({"outcome_unknown":true,"message":"An operation started but its result could not be recorded. Inspect its effects and acknowledge recovery before continuing; it has not been replayed."}))
            } else {
                match outcome {
                    Ok(steps) => (SessionState::Completed, "run.completed", json!({"steps":steps,"message":"The model finished this run. Review changes and observed checks separately."})),
                    Err(AgentError::Interrupted) => (SessionState::Interrupted, "run.interrupted", json!({"message":"The run was interrupted. Recorded changes remain; no automatic rollback or replay occurred."})),
                    Err(error) => (SessionState::Failed, "run.failed", json!({"code":error.kind(),"message":error.to_string()})),
                }
            };
            store::resolve_unmatched_calls(tx, &run.session_id, "This tool did not complete because the run ended. Do not assume it succeeded.")?;
            let mut session = store::get_session(tx, &run.session_id)?;
            session.state = state.clone();
            session.active_run_id = None;
            store::save_session(tx, &session)?;
            store::finish_run(tx, &run.id, state)?;
            store::append_event(tx, &run.session_id, Some(&run.id), kind, payload)?;
            Ok(())
        })
    }
}
