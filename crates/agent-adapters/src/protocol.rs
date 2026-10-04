use crate::{
    AdapterApproval, AdapterError, ApprovalDecision, Control, Result, RunState, StoredRun,
    hash_value,
    process_tree::{self, ProcessTree},
    types::PERMISSION_BOUNDARY,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::mpsc,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const MAX_LINE: usize = 256 * 1024;
const MAX_STREAM: usize = 32 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);

enum Packet {
    Message(Value),
    End,
    Fault(&'static str),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Initialize,
    Thread,
    Turn,
    Active,
    Interrupting,
}
struct Pending {
    wire_id: Value,
    approval: AdapterApproval,
}

pub(crate) struct RunInput {
    pub(crate) project: PathBuf,
    pub(crate) profile: Option<crate::ProfileBinding>,
    pub(crate) prompt: String,
    /// Saved Codex thread to resume instead of starting a new one.
    pub(crate) resume_thread: Option<String>,
    /// Ask Codex not to save a new thread. Used when Switchya keeps no history.
    pub(crate) ephemeral: bool,
}

pub(crate) async fn run(
    executable: PathBuf,
    prefix_args: Vec<String>,
    input: RunInput,
    state: Arc<Mutex<StoredRun>>,
    mut control: mpsc::Receiver<Control>,
    shutdown: CancellationToken,
) {
    let RunInput {
        project,
        profile,
        prompt,
        resume_thread,
        ephemeral,
    } = input;
    let mut command = Command::new(executable);
    command
        .args(prefix_args)
        .args([
            "app-server",
            "--stdio",
            "-c",
            "sandbox_mode=\"read-only\"",
            "-c",
            "approval_policy=\"on-request\"",
            "-c",
            "approvals_reviewer=\"user\"",
        ])
        .current_dir(&project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::profile::configure(&mut command, profile.as_ref());
    process_tree::configure(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            fail(&state, "The Codex process could not start.", false);
            return;
        }
    };
    let mut tree = match ProcessTree::attach(&child) {
        Ok(tree) => tree,
        Err(_) => {
            let _ = child.kill().await;
            fail(
                &state,
                "Could not establish local process cleanup; Codex was stopped.",
                false,
            );
            return;
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        fail(&state, "Codex stdin is unavailable.", false);
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        fail(&state, "Codex stdout is unavailable.", false);
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        fail(&state, "Codex stderr is unavailable.", false);
        return;
    };
    let (packets, mut incoming) = mpsc::channel(16);
    let stdout_task = tokio::spawn(read_stdout(stdout, packets.clone()));
    let stderr_task = tokio::spawn(discard_stderr(stderr, packets));
    let mut stage = Stage::Initialize;
    let mut deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut turn_was_requested = false;
    let mut pending: BTreeMap<String, Pending> = BTreeMap::new();
    let mut items: BTreeMap<String, Value> = BTreeMap::new();
    let initialize = json!({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "switchya_adapter", "title": "Switchya", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {"experimentalApi": false, "explicitGatewayOauth": true}}});
    if write(&mut stdin, &initialize).await.is_err() {
        fail(&state, "Could not initialize Codex transport.", false);
    } else {
        'transport: loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    fail(&state, "The local host shut down. This run will not be replayed.", turn_was_requested);
                    break;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let message = if stage == Stage::Interrupting { "Codex did not confirm interruption within five seconds; its process tree was stopped. Check any approved operation before retrying." } else { "Codex exceeded the adapter time limit; its process tree was stopped." };
                    fail(&state, message, turn_was_requested);
                    break;
                }
                next = control.recv() => match next {
                    None => { fail(&state, "Adapter control closed.", turn_was_requested); break; }
                    Some(Control::Decide { approval_id, expected_hash, decision, reply }) => {
                        let result = decide(&mut stdin, &state, &mut pending, &approval_id, &expected_hash, decision, stage == Stage::Interrupting).await;
                        let transport_failed = matches!(result, Err(AdapterError::Protocol(_)));
                        let _ = reply.send(result);
                        if transport_failed { fail(&state, "Approval response delivery was uncertain; the run was stopped and will not be replayed.", true); break; }
                    }
                    Some(Control::Interrupt { reply }) => {
                        if stage == Stage::Interrupting { let _ = reply.send(Ok(())); continue; }
                        let view = state.lock().view.clone();
                        if !turn_was_requested {
                            state.lock().state(RunState::Interrupted);
                            let _ = reply.send(Ok(()));
                            break;
                        }
                        state.lock().state(RunState::Interrupting);
                        for approval in pending.values() {
                            let _ = write(&mut stdin, &json!({"id": approval.wire_id, "result": {"decision": "cancel"}})).await;
                        }
                        pending.clear();
                        state.lock().view.pending_approvals.clear();
                        if let (Some(thread), Some(turn)) = (view.thread_id, view.turn_id) {
                            let result = write(&mut stdin, &json!({"id":4, "method":"turn/interrupt", "params":{"threadId":thread, "turnId":turn}})).await;
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed { fail(&state, "Interruption delivery failed; check any approved operation before retrying.", true); break; }
                            stage = Stage::Interrupting;
                            deadline = Instant::now() + INTERRUPT_TIMEOUT;
                        } else {
                            let _ = reply.send(Ok(()));
                            fail(&state, "Stopped before Codex confirmed the turn identifier. The outcome is uncertain; this run will not be replayed.", true);
                            break;
                        }
                    }
                },
                packet = incoming.recv() => {
                    let message = match packet {
                        Some(Packet::Message(value)) => value,
                        Some(Packet::Fault(reason)) => { fail(&state, reason, turn_was_requested); break; }
                        Some(Packet::End) | None => { fail(&state, "Codex exited without a confirmed turn completion. No work will be replayed.", turn_was_requested); break; }
                    };
                    if message.get("method").is_some() && message.get("id").is_some() {
                        if let Err(error) = server_request(&mut stdin, &state, &mut pending, &items, &message, stage == Stage::Interrupting).await {
                            fail(&state, &error.to_string(), turn_was_requested); break;
                        }
                        continue;
                    }
                    if let Some(method) = message.get("method").and_then(Value::as_str) {
                        if method == "serverRequest/resolved" {
                            let params = message.get("params").unwrap_or(&Value::Null);
                            if params.get("threadId").and_then(Value::as_str) == state.lock().view.thread_id.as_deref() {
                                let resolved: Vec<_> = pending.iter().filter(|(_, p)| Some(&p.wire_id) == params.get("requestId")).map(|(id, _)| id.clone()).collect();
                                for id in resolved {
                                    pending.remove(&id);
                                    let mut run = state.lock();
                                    run.view.pending_approvals.retain(|p| p.id != id);
                                    run.event("approval_resolved", json!({"approval_id":id,"decision":"resolved_by_cli"}));
                                    if pending.is_empty() && run.view.state == RunState::AwaitingApproval { run.state(RunState::Running); }
                                }
                            }
                            continue;
                        }
                        if notification(&state, &mut items, method, message.get("params").unwrap_or(&Value::Null), turn_was_requested) { break; }
                        continue;
                    }
                    let expected_id = match stage { Stage::Initialize => 1, Stage::Thread => 2, Stage::Turn => 3, Stage::Interrupting => 4, Stage::Active => 0 };
                    if message.get("id").and_then(Value::as_u64) != Some(expected_id) { continue; }
                    if message.get("error").is_some() {
                        let code = message.pointer("/error/code").and_then(Value::as_i64);
                        // RPC error messages can contain credentials or arbitrary runtime logs.
                        fail(&state, &format!("Codex rejected the protocol request (code {code:?}). Check the installed CLI version, sign-in and configuration in its own terminal."), stage == Stage::Interrupting);
                        break;
                    }
                    let Some(result) = message.get("result") else { fail(&state, "Codex returned a malformed protocol response.", turn_was_requested); break; };
                    let next = match stage {
                        Stage::Initialize => {
                            if !crate::profile::confirmed_home(result, profile.as_ref()) { fail(&state, "Codex did not confirm the selected account directory. No model turn was started.", false); break; }
                            if write(&mut stdin, &json!({"method":"initialized"})).await.is_err() { fail(&state, "Could not finish Codex initialization.", false); break; }
                            stage = Stage::Thread;
                            match &resume_thread {
                                // Switchya keeps its own transcript; ask only for thread state.
                                Some(thread) => json!({"id":2,"method":"thread/resume","params":{"threadId":thread,"cwd":project,"approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":"read-only","excludeTurns":true}}),
                                None => json!({"id":2,"method":"thread/start","params":{"cwd":project,"approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":"read-only","ephemeral":ephemeral}}),
                            }
                        }
                        Stage::Thread => {
                            if !confirmed_policy(result) { fail(&state, "Codex did not confirm the required read-only sandbox, disabled shell network and human approval policy. No model turn was started.", false); break; }
                            let Some(thread) = result.pointer("/thread/id").and_then(Value::as_str).filter(|s| valid_id(s)) else { fail(&state, "Codex did not return a valid thread identifier.", false); break; };
                            if resume_thread.as_deref().is_some_and(|expected| expected != thread) { fail(&state, "Codex resumed a different conversation than the one requested. No model turn was started.", false); break; }
                            {
                                let mut run = state.lock();
                                run.view.thread_id = Some(thread.into());
                                run.view.model = result.get("model").and_then(Value::as_str).map(|s| s.chars().take(200).collect());
                                let message = if resume_thread.is_some() { "Codex resumed the saved conversation and confirmed its read-only shell policy and human approval reviewer. Only your new message is sent." } else { "Codex confirmed its read-only shell policy and human approval reviewer. The CLI-configured model will be used." };
                                run.event("notice", json!({"message":message}));
                            }
                            stage = Stage::Turn;
                            // No side-effecting model request is sent until policy read-back succeeds.
                            turn_was_requested = true;
                            json!({"id":3,"method":"turn/start","params":{"threadId":thread,"input":[{"type":"text","text":prompt}],"approvalPolicy":"on-request","approvalsReviewer":"user","sandboxPolicy":{"type":"readOnly","networkAccess":false}}})
                        }
                        Stage::Turn => {
                            let Some(turn) = result.pointer("/turn/id").and_then(Value::as_str).filter(|s| valid_id(s)) else { fail(&state, "Codex did not return a valid turn identifier.", true); break; };
                            {
                                let mut run = state.lock();
                                if run.view.turn_id.as_deref().is_some_and(|id| id != turn) { fail_unlocked(&mut run, "Codex returned conflicting turn identifiers.", true); break 'transport; }
                                run.view.turn_id = Some(turn.into());
                                if pending.is_empty() { run.state(RunState::Running); }
                            }
                            stage = Stage::Active;
                            deadline = Instant::now() + TURN_TIMEOUT;
                            continue;
                        }
                        Stage::Interrupting => { continue; } // A response acknowledges receipt; completion confirms interruption.
                        Stage::Active => { continue; }
                    };
                    deadline = Instant::now() + HANDSHAKE_TIMEOUT;
                    if write(&mut stdin, &next).await.is_err() { fail(&state, "Codex request delivery failed.", turn_was_requested); break; }
                }
            }
        }
    }
    drop(stdin);
    let terminated = tree.terminate().is_ok();
    let _ = child.start_kill();
    let reaped = matches!(
        tokio::time::timeout(Duration::from_secs(5), child.wait()).await,
        Ok(Ok(_))
    );
    stdout_task.abort();
    stderr_task.abort();
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    if !terminated || !reaped {
        fail(
            &state,
            "Process cleanup could not be confirmed. Inspect Codex processes before starting another run.",
            true,
        );
    }
}

fn confirmed_policy(result: &Value) -> bool {
    result.get("approvalPolicy").and_then(Value::as_str) == Some("on-request")
        && result.get("approvalsReviewer").and_then(Value::as_str) == Some("user")
        && result.pointer("/sandbox/type").and_then(Value::as_str) == Some("readOnly")
        && result
            .pointer("/sandbox/networkAccess")
            .is_none_or(|v| v.as_bool() == Some(false))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

async fn write(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|_| AdapterError::Protocol("Could not encode protocol request.".into()))?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), stdin.write_all(&bytes))
        .await
        .map_err(|_| AdapterError::Protocol("Codex protocol write timed out.".into()))?
        .map_err(|_| AdapterError::Protocol("Codex protocol write failed.".into()))
}

async fn read_stdout(stdout: tokio::process::ChildStdout, packets: mpsc::Sender<Packet>) {
    let mut reader = BufReader::new(stdout);
    let mut total = 0usize;
    loop {
        let mut line = Vec::new();
        let n = match (&mut reader)
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(n) => n,
            Err(_) => {
                let _ = packets
                    .send(Packet::Fault("Codex stdout could not be read."))
                    .await;
                break;
            }
        };
        if n == 0 {
            let _ = packets.send(Packet::End).await;
            break;
        }
        total = total.saturating_add(n);
        if n > MAX_LINE || total > MAX_STREAM {
            let _ = packets
                .send(Packet::Fault(
                    "Codex exceeded the bounded protocol output limit.",
                ))
                .await;
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(message) if message.is_object() => {
                if packets.send(Packet::Message(message)).await.is_err() {
                    break;
                }
            }
            _ => {
                let _ = packets
                    .send(Packet::Fault(
                        "Codex emitted invalid structured JSON output.",
                    ))
                    .await;
                break;
            }
        }
    }
}

async fn discard_stderr(mut stderr: tokio::process::ChildStderr, packets: mpsc::Sender<Packet>) {
    let mut buffer = [0u8; 8192];
    let mut total = 0usize;
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n;
                if total > MAX_STREAM {
                    let _ = packets
                        .send(Packet::Fault("Codex exceeded the diagnostic output limit."))
                        .await;
                    break;
                }
            }
        }
    }
}

async fn server_request(
    stdin: &mut ChildStdin,
    state: &Arc<Mutex<StoredRun>>,
    pending: &mut BTreeMap<String, Pending>,
    items: &BTreeMap<String, Value>,
    message: &Value,
    interrupting: bool,
) -> Result<()> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Protocol("Invalid server method.".into()))?;
    let id = message
        .get("id")
        .filter(|id| id.as_i64().is_some() || id.as_str().is_some_and(valid_id))
        .ok_or_else(|| AdapterError::Protocol("Invalid server request identifier.".into()))?;
    if !matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    ) {
        // Unknown requests, broad permission grants, MCP elicitation, auth refresh and
        // dynamic tools are never auto-approved or executed by this host.
        write(stdin, &json!({"id":id,"error":{"code":-32601,"message":"This request is not supported by the Switchya adapter."}})).await?;
        state.lock().event("notice", json!({"message":"An unsupported Codex request was declined. This adapter handles only command and file-change decisions."}));
        return Ok(());
    }
    if interrupting {
        return write(stdin, &json!({"id":id,"result":{"decision":"cancel"}})).await;
    }
    let params = message
        .get("params")
        .ok_or_else(|| AdapterError::Protocol("Approval has no parameters.".into()))?;
    let thread = state.lock().view.thread_id.clone();
    let turn = state.lock().view.turn_id.clone();
    if thread.is_none()
        || params.get("threadId").and_then(Value::as_str) != thread.as_deref()
        || !params
            .get("turnId")
            .and_then(Value::as_str)
            .is_some_and(valid_id)
        || !params
            .get("itemId")
            .and_then(Value::as_str)
            .is_some_and(valid_id)
        || turn
            .as_deref()
            .is_some_and(|turn| params.get("turnId").and_then(Value::as_str) != Some(turn))
    {
        return Err(AdapterError::Protocol(
            "Approval did not belong to the active thread and turn.".into(),
        ));
    }
    if turn.is_none() {
        state.lock().view.turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    if pending.values().any(|p| &p.wire_id == id) || pending.len() >= 16 {
        return Err(AdapterError::Protocol(
            "Duplicate or excessive pending approval requests.".into(),
        ));
    }
    let item = params
        .get("itemId")
        .and_then(Value::as_str)
        .and_then(|id| items.get(id))
        .cloned();
    let can_allow_once = if method == "item/commandExecution/requestApproval" {
        params
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
            && params
                .get("cwd")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
    } else {
        // grantRoot can expand permission for the remaining session. Do not expose
        // it as a one-use operation, and never approve a patch without its changes.
        params.get("grantRoot").is_none_or(Value::is_null)
            && item
                .as_ref()
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                == Some("fileChange")
            && item
                .as_ref()
                .and_then(|i| i.get("changes"))
                .and_then(Value::as_array)
                .is_some_and(|changes| !changes.is_empty())
    };
    let preview = json!({"params":params,"item":item});
    let approval = AdapterApproval {
        id: format!("approval_{}", uuid::Uuid::new_v4().simple()),
        expected_hash: hash_value(&json!({"method":method,"request_id":id,"preview":preview})),
        method: method.into(),
        preview,
        can_allow_once,
        permission_boundary: PERMISSION_BOUNDARY.into(),
    };
    pending.insert(
        approval.id.clone(),
        Pending {
            wire_id: id.clone(),
            approval: approval.clone(),
        },
    );
    let mut run = state.lock();
    run.view.pending_approvals.push(approval.clone());
    run.event("approval_requested", json!({"approval":approval}));
    run.state(RunState::AwaitingApproval);
    Ok(())
}

async fn decide(
    stdin: &mut ChildStdin,
    state: &Arc<Mutex<StoredRun>>,
    pending: &mut BTreeMap<String, Pending>,
    approval_id: &str,
    expected_hash: &str,
    decision: ApprovalDecision,
    interrupting: bool,
) -> Result<()> {
    if interrupting {
        return Err(AdapterError::Conflict("The run is interrupting.".into()));
    }
    let request = pending
        .get(approval_id)
        .ok_or_else(|| AdapterError::Conflict("Approval is no longer pending.".into()))?;
    if request.approval.expected_hash != expected_hash {
        return Err(AdapterError::Conflict(
            "Approval details changed or the supplied hash is stale.".into(),
        ));
    }
    if decision == ApprovalDecision::AllowOnce && !request.approval.can_allow_once {
        return Err(AdapterError::Invalid(
            "This request lacks a complete single-operation preview; only denial is supported."
                .into(),
        ));
    }
    let result = match decision {
        ApprovalDecision::AllowOnce => "accept",
        ApprovalDecision::Deny => "decline",
    };
    write(
        stdin,
        &json!({"id":request.wire_id,"result":{"decision":result}}),
    )
    .await?;
    pending.remove(approval_id);
    let mut run = state.lock();
    run.view.pending_approvals.retain(|p| p.id != approval_id);
    run.event(
        "approval_resolved",
        json!({"approval_id":approval_id,"decision":decision}),
    );
    if pending.is_empty() {
        run.state(RunState::Running);
    }
    Ok(())
}

fn notification(
    state: &Arc<Mutex<StoredRun>>,
    items: &mut BTreeMap<String, Value>,
    method: &str,
    params: &Value,
    turn_requested: bool,
) -> bool {
    if !turn_requested {
        return false;
    }
    let mut run = state.lock();
    if params.get("threadId").and_then(Value::as_str) != run.view.thread_id.as_deref() {
        return false;
    }
    if let Some(turn) = params.get("turnId").and_then(Value::as_str)
        && run
            .view
            .turn_id
            .as_deref()
            .is_some_and(|active| active != turn)
    {
        return false;
    }
    match method {
        "turn/started" => {
            if let Some(turn) = params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .filter(|s| valid_id(s))
                && run
                    .view
                    .turn_id
                    .as_deref()
                    .is_none_or(|active| active == turn)
            {
                run.view.turn_id = Some(turn.into());
            }
        }
        "item/agentMessage/delta" => {
            if let Some(text) = params.get("delta").and_then(Value::as_str) {
                run.event(
                    "assistant_delta",
                    json!({"item_id":params.get("itemId"),"text":text}),
                );
            }
        }
        "item/started" | "item/completed" => {
            if let Some(item) = params.get("item") {
                let filtered = public_item(item);
                if let Some(id) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| valid_id(s))
                {
                    if method == "item/started" && items.len() < 128 {
                        items.insert(id.into(), filtered.clone());
                    }
                    if method == "item/completed" {
                        items.remove(id);
                    }
                }
                run.event(
                    if method == "item/started" {
                        "item_started"
                    } else {
                        "item_completed"
                    },
                    json!({"item":filtered}),
                );
            }
        }
        "turn/completed" => {
            let Some(turn) = params.get("turn") else {
                return false;
            };
            if !turn.get("id").and_then(Value::as_str).is_some_and(valid_id) {
                return false;
            }
            if run
                .view
                .turn_id
                .as_deref()
                .is_some_and(|id| turn.get("id").and_then(Value::as_str) != Some(id))
            {
                return false;
            }
            let status = turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            run.event("turn_completed", json!({"status":status}));
            run.state(match status {
                "completed" => RunState::Completed,
                "interrupted" => RunState::Interrupted,
                "failed" => RunState::Failed,
                _ => RunState::RecoveryRequired,
            });
            return true;
        }
        _ => {} // Account, config, environment and unknown notifications are not forwarded.
    }
    false
}

fn public_item(item: &Value) -> Value {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let fields: &[&str] = match kind {
        "agentMessage" => &["id", "type", "text"],
        "commandExecution" => &[
            "id",
            "type",
            "command",
            "cwd",
            "status",
            "aggregatedOutput",
            "exitCode",
        ],
        "fileChange" => &["id", "type", "changes", "status"],
        _ => &["id", "type", "status"],
    };
    let mut result = serde_json::Map::new();
    for field in fields {
        if let Some(value) = item.get(*field) {
            result.insert((*field).into(), value.clone());
        }
    }
    Value::Object(result)
}

fn fail(state: &Arc<Mutex<StoredRun>>, message: &str, uncertain: bool) {
    fail_unlocked(&mut state.lock(), message, uncertain);
}
fn fail_unlocked(state: &mut StoredRun, message: &str, uncertain: bool) {
    state.event("adapter_error", json!({"message":message}));
    state.state(if uncertain {
        RunState::RecoveryRequired
    } else {
        RunState::Failed
    });
}
