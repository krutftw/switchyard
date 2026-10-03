//! Real gateway + independent local provider wire fixture. No remote API calls.
#![allow(dead_code)]

#[path = "../../gateway/tests/support/fake.rs"]
mod fake;

use fake::{Answer, Behaviour, Fake, Kind};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;
use switchyard_agent::{AppEngine, Decision, EngineOptions, Session, SessionState};
use switchyard_gateway::{Gateway, GatewayOptions, PresentedCredentials};
use tempfile::TempDir;

const CLIENT_KEY: &str = "sy-agent-test-client-key-0123456789";
const UPSTREAM_KEY: &str = "local-scripted-provider-key";

struct Harness {
    temp: TempDir,
    fake: Fake,
    gateway: Gateway,
    engine: AppEngine,
    session: Session,
}

impl Harness {
    async fn new(protocol: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let fake = Fake::start().await;
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("hello.txt"), "initial content\n").unwrap();
        let config_path = temp.path().join("switchyard.toml");
        let (kind, wire, base) = if protocol == "anthropic" {
            ("anthropic", "", fake.base())
        } else {
            (
                "openai",
                "wire_api = \"responses\"",
                format!("{}/v1", fake.base()),
            )
        };
        std::fs::write(
            &config_path,
            format!(
                r#"
[upstream]
proxy = "direct"
[routing]
max_attempts = 1
[[auth.keys]]
key = "{CLIENT_KEY}"
name = "app"
models = ["fixture"]
[[providers]]
name = "fixture"
kind = "{kind}"
{wire}
base_url = "{base}"
api_keys = ["{UPSTREAM_KEY}"]
discover = false
[[providers.models]]
id = "fixture"
"#
            ),
        )
        .unwrap();
        let gateway = Gateway::start(GatewayOptions::new(config_path).watch(false))
            .await
            .unwrap();
        let identity = gateway
            .authenticate(&PresentedCredentials {
                x_api_key: Some(CLIENT_KEY.into()),
                ..Default::default()
            })
            .unwrap();
        let engine = AppEngine::open(
            gateway.clone(),
            identity,
            EngineOptions::new(temp.path().join("state")),
        )
        .unwrap();
        let project = engine.open_project(&project).unwrap();
        let session = engine.create_session(&project.id, "fixture").unwrap();
        Self {
            temp,
            fake,
            gateway,
            engine,
            session,
        }
    }

    async fn wait(&self, state: SessionState) {
        self.wait_session(&self.session.id, state).await;
    }

    async fn wait_session(&self, session_id: &str, state: SessionState) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let view = self.engine.session(session_id).unwrap();
                if view.session.state == state {
                    break;
                }
                if matches!(
                    view.session.state,
                    SessionState::Failed | SessionState::RecoveryRequired
                ) && view.session.state != state
                {
                    panic!(
                        "unexpected state {:?}: {:?}",
                        view.session.state, view.events
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("session must reach expected state");
    }

    fn generations(&self) -> Vec<fake::Recorded> {
        self.fake
            .requests()
            .into_iter()
            .filter(|r| matches!(r.kind, Kind::Generate { .. }))
            .collect()
    }

    async fn close(&self) {
        self.engine.shutdown().await.unwrap();
        self.gateway.shutdown().await;
    }
}

#[tokio::test]
async fn real_gateway_read_tool_loop_is_durable_and_command_retries_are_idempotent() {
    let h = Harness::new("responses").await;
    h.fake.script(
        UPSTREAM_KEY,
        [
            Behaviour::Reply(Answer::tool("read_file", json!({"path":"hello.txt"}))),
            Behaviour::text("Read the file; no changes."),
        ],
    );
    let run = h
        .engine
        .submit_turn(&h.session.id, "read-1", "Read hello.txt")
        .unwrap();
    assert_eq!(
        h.engine
            .submit_turn(&h.session.id, "read-1", "Read hello.txt")
            .unwrap()
            .id,
        run.id
    );
    assert!(
        h.engine
            .submit_turn(&h.session.id, "read-1", "Changed text")
            .is_err()
    );
    h.wait(SessionState::Completed).await;
    assert_eq!(
        h.engine
            .submit_turn(&h.session.id, "read-1", "Read hello.txt")
            .unwrap()
            .id,
        run.id
    );
    let requests = h.generations();
    assert_eq!(requests.len(), 2);
    let outputs: Vec<_> = requests[1].body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 1);
    assert!(
        outputs[0]["output"]
            .as_str()
            .unwrap()
            .contains("initial content")
    );
    let events = h.engine.events(&h.session.id, 0, 500).unwrap();
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64 + 1);
    }
    assert_eq!(events.last().unwrap().kind, "run.completed");
    let public = serde_json::to_string(&h.engine.session(&h.session.id).unwrap()).unwrap();
    assert!(!public.contains(CLIENT_KEY));
    assert!(!public.contains(UPSTREAM_KEY));
    assert_eq!(
        h.engine
            .events(&h.session.id, events.last().unwrap().seq, 200)
            .unwrap()
            .len(),
        0
    );
    h.close().await;
}

fn patch() -> Answer {
    Answer::tool(
        "apply_patch",
        json!({"edits":[{"path":"hello.txt","before_sha256":format!("{:x}",Sha256::digest(b"initial content\n")),"content":"reviewed edit\n"}]}),
    )
}

#[tokio::test]
async fn patch_requires_exact_single_use_approval_and_records_change_evidence() {
    let h = Harness::new("responses").await;
    h.fake.script(
        UPSTREAM_KEY,
        [
            Behaviour::Reply(patch()),
            Behaviour::text("Updated hello.txt; no tests were run."),
        ],
    );
    h.engine
        .submit_turn(&h.session.id, "patch-1", "Edit the file")
        .unwrap();
    h.wait(SessionState::AwaitingApproval).await;
    let operation = h
        .engine
        .session(&h.session.id)
        .unwrap()
        .pending_operation
        .unwrap();
    let path = h.temp.path().join("project/hello.txt");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "initial content\n");
    assert!(
        h.engine
            .decide_operation(
                &h.session.id,
                &operation.id,
                "wrong-hash",
                Decision::AllowOnce
            )
            .is_err()
    );
    assert!(
        h.engine
            .submit_turn(&h.session.id, "other", "Another turn")
            .is_err()
    );
    h.engine
        .decide_operation(
            &h.session.id,
            &operation.id,
            &operation.arguments_hash,
            Decision::AllowOnce,
        )
        .unwrap();
    assert!(
        h.engine
            .decide_operation(
                &h.session.id,
                &operation.id,
                &operation.arguments_hash,
                Decision::AllowOnce
            )
            .is_err()
    );
    h.wait(SessionState::Completed).await;
    assert_eq!(std::fs::read_to_string(path).unwrap(), "reviewed edit\n");
    let events = h.engine.events(&h.session.id, 0, 500).unwrap();
    let completed = events
        .iter()
        .find(|event| event.kind == "tool.completed")
        .unwrap();
    assert_eq!(
        completed.payload["result"]["changes"][0]["path"],
        "hello.txt"
    );
    assert_eq!(completed.payload["result"]["status"], "completed");
    h.close().await;
}

#[tokio::test]
async fn overlapping_project_mutations_serialize_and_recheck_hashes_while_reads_continue() {
    let h = Harness::new("responses").await;
    let project = h.temp.path().join("project");
    let nested = project.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let path = nested.join("hello.txt");
    std::fs::write(&path, "initial content\n").unwrap();
    let nested_project = h.engine.open_project(&nested).unwrap();
    assert_ne!(nested_project.id, h.session.project_id);
    let patch_session = h
        .engine
        .create_session(&nested_project.id, "fixture")
        .unwrap();
    let read_session = h
        .engine
        .create_session(&h.session.project_id, "fixture")
        .unwrap();

    // Release the bounded command even if an assertion fails before cleanup.
    struct ReleaseCommand(std::path::PathBuf);
    impl Drop for ReleaseCommand {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "release");
        }
    }
    let release = ReleaseCommand(project.join("release-command"));
    #[cfg(windows)]
    let command = r#"
$ErrorActionPreference = 'Stop'
[IO.File]::WriteAllText((Join-Path (Get-Location) 'command-ready'), 'ready')
while (-not (Test-Path -LiteralPath 'release-command')) { Start-Sleep -Milliseconds 10 }
[IO.File]::WriteAllText((Join-Path (Get-Location) 'nested/hello.txt'), "first change`n")
"#;
    #[cfg(unix)]
    let command = r#"
printf ready > command-ready
while [ ! -e release-command ]; do sleep 0.01; done
printf 'first change\n' > nested/hello.txt
"#;
    h.fake.script(
        UPSTREAM_KEY,
        [
            Behaviour::Reply(Answer::tool(
                "run_command",
                json!({"command":command,"timeout_ms":30_000}),
            )),
            Behaviour::Reply(patch()),
            Behaviour::Reply(Answer::tool(
                "read_file",
                json!({"path":"nested/hello.txt"}),
            )),
        ],
    );
    h.fake
        .always(UPSTREAM_KEY, Behaviour::text("Reported the tool result."));
    h.engine
        .submit_turn(&h.session.id, "hold-mutation", "Make the first change")
        .unwrap();
    h.wait(SessionState::AwaitingApproval).await;
    let operation = h
        .engine
        .session(&h.session.id)
        .unwrap()
        .pending_operation
        .unwrap();
    h.engine
        .decide_operation(
            &h.session.id,
            &operation.id,
            &operation.arguments_hash,
            Decision::AllowOnce,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !project.join("command-ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approved command must reach its barrier");

    h.engine
        .submit_turn(&patch_session.id, "queued-patch", "Make the second change")
        .unwrap();
    h.wait_session(&patch_session.id, SessionState::AwaitingApproval)
        .await;
    // Keep the patch awaiting approval until the reader has consumed its
    // scripted response: all sessions share the fixture's provider queue.
    h.engine
        .submit_turn(&read_session.id, "concurrent-read", "Read the shared file")
        .unwrap();
    h.wait_session(&read_session.id, SessionState::Completed)
        .await;
    let read_events = h.engine.events(&read_session.id, 0, 500).unwrap();
    let read = read_events
        .iter()
        .find(|event| event.kind == "tool.completed")
        .unwrap();
    assert_eq!(read.payload["name"], "read_file");
    assert_eq!(read.payload["result"]["status"], "completed");
    assert_eq!(
        read.payload["result"]["output"]["content"],
        "initial content\n"
    );

    let operation = h
        .engine
        .session(&patch_session.id)
        .unwrap()
        .pending_operation
        .unwrap();
    h.engine
        .decide_operation(
            &patch_session.id,
            &operation.id,
            &operation.arguments_hash,
            Decision::AllowOnce,
        )
        .unwrap();
    let started_early = tokio::time::timeout(Duration::from_millis(350), async {
        loop {
            if h.engine
                .events(&patch_session.id, 0, 500)
                .unwrap()
                .iter()
                .any(|event| event.kind == "tool.started")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    let held_content = std::fs::read_to_string(&path).unwrap();
    drop(release);
    h.wait(SessionState::Completed).await;
    h.wait_session(&patch_session.id, SessionState::Completed)
        .await;
    assert!(
        !started_early,
        "nested project mutation bypassed the active command"
    );
    assert_eq!(held_content, "initial content\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first change\n");
    let command_events = h.engine.events(&h.session.id, 0, 500).unwrap();
    let command = command_events
        .iter()
        .find(|event| event.kind == "tool.completed")
        .unwrap();
    assert_eq!(command.payload["result"]["status"], "completed");
    assert_eq!(command.payload["result"]["exit_code"], 0);
    let patch_events = h.engine.events(&patch_session.id, 0, 500).unwrap();
    let patch = patch_events
        .iter()
        .find(|event| event.kind == "tool.completed")
        .unwrap();
    assert_eq!(patch.payload["result"]["status"], "failed");
    assert_eq!(patch.payload["result"]["changes"], json!([]));
    assert!(
        patch.payload["result"]["error"]
            .as_str()
            .unwrap()
            .contains("changed since approval")
    );
    h.close().await;
}

#[tokio::test]
async fn denying_a_patch_returns_denial_to_model_without_writing() {
    let h = Harness::new("responses").await;
    h.fake.script(
        UPSTREAM_KEY,
        [
            Behaviour::Reply(patch()),
            Behaviour::text("The change was denied."),
        ],
    );
    h.engine
        .submit_turn(&h.session.id, "deny-1", "Edit the file")
        .unwrap();
    h.wait(SessionState::AwaitingApproval).await;
    let operation = h
        .engine
        .session(&h.session.id)
        .unwrap()
        .pending_operation
        .unwrap();
    h.engine
        .decide_operation(
            &h.session.id,
            &operation.id,
            &operation.arguments_hash,
            Decision::Deny,
        )
        .unwrap();
    h.wait(SessionState::Completed).await;
    assert_eq!(
        std::fs::read_to_string(h.temp.path().join("project/hello.txt")).unwrap(),
        "initial content\n"
    );
    assert!(h.generations()[1].body.to_string().contains("denied"));
    h.close().await;
}

#[tokio::test]
async fn interrupt_expires_pending_approval_and_next_turn_can_continue() {
    let h = Harness::new("responses").await;
    h.fake.script(
        UPSTREAM_KEY,
        [
            Behaviour::Reply(patch()),
            Behaviour::text("Continued safely."),
        ],
    );
    let run = h
        .engine
        .submit_turn(&h.session.id, "interrupt-1", "Edit the file")
        .unwrap();
    h.wait(SessionState::AwaitingApproval).await;
    let operation = h
        .engine
        .session(&h.session.id)
        .unwrap()
        .pending_operation
        .unwrap();
    h.engine.interrupt(&h.session.id, &run.id).unwrap();
    h.wait(SessionState::Interrupted).await;
    assert!(
        h.engine
            .decide_operation(
                &h.session.id,
                &operation.id,
                &operation.arguments_hash,
                Decision::AllowOnce
            )
            .is_err()
    );
    assert!(
        h.engine
            .session(&h.session.id)
            .unwrap()
            .pending_operation
            .is_none()
    );
    assert_eq!(
        std::fs::read_to_string(h.temp.path().join("project/hello.txt")).unwrap(),
        "initial content\n"
    );
    // Wait for active-run cleanup as completion is journalled immediately before
    // removal of the actor. The API may correctly return a transient conflict.
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.engine
        .submit_turn(&h.session.id, "interrupt-2", "Just explain what happened")
        .unwrap();
    h.wait(SessionState::Completed).await;
    assert!(
        h.generations()[1]
            .body
            .to_string()
            .contains("did not complete")
    );
    h.close().await;
}

#[tokio::test]
async fn incomplete_model_stream_never_executes_a_tool() {
    let h = Harness::new("responses").await;
    h.fake.script(
        UPSTREAM_KEY,
        [Behaviour::EndAfter {
            frames: 3,
            answer: patch(),
        }],
    );
    h.engine
        .submit_turn(&h.session.id, "truncated-1", "Edit the file")
        .unwrap();
    h.wait(SessionState::Failed).await;
    assert!(
        h.engine
            .session(&h.session.id)
            .unwrap()
            .pending_operation
            .is_none()
    );
    assert_eq!(
        std::fs::read_to_string(h.temp.path().join("project/hello.txt")).unwrap(),
        "initial content\n"
    );
    assert!(
        !h.engine
            .events(&h.session.id, 0, 500)
            .unwrap()
            .iter()
            .any(|event| event.kind == "tool.started")
    );
    h.close().await;
}

#[tokio::test]
async fn anonymous_and_privileged_dashboard_identities_cannot_run_the_app_engine() {
    let h = Harness::new("responses").await;
    assert!(
        AppEngine::open(
            h.gateway.clone(),
            h.gateway.dashboard_identity(),
            EngineOptions::new(h.temp.path().join("other"))
        )
        .is_err()
    );
    assert!(
        h.engine
            .create_session(&h.session.project_id, "invented-model")
            .is_err()
    );
    h.close().await;
}

#[tokio::test]
async fn interrupt_cancels_gateway_wait_before_any_model_output() {
    let h = Harness::new("responses").await;
    h.fake.script(UPSTREAM_KEY, [Behaviour::Silence]);
    let run = h
        .engine
        .submit_turn(&h.session.id, "silence-1", "Wait for the provider")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.generations().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    h.engine.interrupt(&h.session.id, &run.id).unwrap();
    h.wait(SessionState::Interrupted).await;
    assert_eq!(h.generations().len(), 1);
    h.close().await;
}

#[tokio::test]
async fn model_step_limit_stops_a_tool_loop_without_unbounded_requests() {
    let h = Harness::new("responses").await;
    let Harness {
        temp,
        fake,
        gateway,
        engine,
        session,
    } = h;
    drop(engine);
    let identity = gateway
        .authenticate(&PresentedCredentials {
            x_api_key: Some(CLIENT_KEY.into()),
            ..Default::default()
        })
        .unwrap();
    let mut options = EngineOptions::new(temp.path().join("state"));
    options.max_steps = 1;
    let engine = AppEngine::open(gateway.clone(), identity, options).unwrap();
    fake.script(
        UPSTREAM_KEY,
        [Behaviour::Reply(Answer::tool(
            "read_file",
            json!({"path":"hello.txt"}),
        ))],
    );
    engine
        .submit_turn(&session.id, "bounded-1", "Read the file")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while engine.session(&session.id).unwrap().session.state != SessionState::Failed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events = engine.events(&session.id, 0, 500).unwrap();
    assert_eq!(events.last().unwrap().payload["code"], "limit_reached");
    assert_eq!(
        fake.requests()
            .into_iter()
            .filter(|r| matches!(r.kind, Kind::Generate { .. }))
            .count(),
        1
    );
    engine.shutdown().await.unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn restarted_engine_preserves_foreign_reasoning_signatures_without_gateway_memory() {
    let h = Harness::new("anthropic").await;
    h.fake.script(
        UPSTREAM_KEY,
        [
            Behaviour::Reply(
                Answer::tool("read_file", json!({"path":"hello.txt"}))
                    .with_reasoning("private reasoning", "durable-provider-signature"),
            ),
            Behaviour::text("Read the file."),
        ],
    );
    h.engine
        .submit_turn(&h.session.id, "restart-1", "Read hello.txt")
        .unwrap();
    h.wait(SessionState::Completed).await;
    let session_id = h.session.id.clone();
    h.close().await;
    let Harness {
        temp,
        fake,
        gateway,
        engine,
        session: _,
    } = h;
    drop(engine);
    drop(gateway);
    let gateway =
        Gateway::start(GatewayOptions::new(temp.path().join("switchyard.toml")).watch(false))
            .await
            .unwrap();
    let identity = gateway
        .authenticate(&PresentedCredentials {
            x_api_key: Some(CLIENT_KEY.into()),
            ..Default::default()
        })
        .unwrap();
    let engine = AppEngine::open(
        gateway.clone(),
        identity,
        EngineOptions::new(temp.path().join("state")),
    )
    .unwrap();
    fake.script(
        UPSTREAM_KEY,
        [Behaviour::text("The durable session continued.")],
    );
    engine
        .submit_turn(&session_id, "restart-2", "Continue from the prior turn")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while engine.session(&session_id).unwrap().session.state != SessionState::Completed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request = fake.last();
    assert!(
        request
            .body
            .to_string()
            .contains("durable-provider-signature"),
        "{}",
        request.body
    );
    let public = serde_json::to_string(&engine.session(&session_id).unwrap()).unwrap();
    assert!(!public.contains("durable-provider-signature"));
    assert!(!public.contains("private reasoning"));
    engine.shutdown().await.unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn human_recovery_review_requires_a_new_turn_and_does_not_replay_unknown_command() {
    use switchyard_agent::{Operation, Run};
    let h = Harness::new("responses").await;
    let Harness {
        temp,
        fake,
        gateway,
        engine,
        mut session,
    } = h;
    engine.shutdown().await.unwrap();
    drop(engine);
    // Simulate a process that died after journalling command start, before it
    // could record an outcome. This creates no actual shell process.
    let connection =
        rusqlite::Connection::open(temp.path().join("state/sessions.sqlite3")).unwrap();
    session.state = SessionState::Running;
    session.active_run_id = Some("crashed_run".into());
    let run = Run {
        id: "crashed_run".into(),
        session_id: session.id.clone(),
        command_id: "crashed_command".into(),
        state: SessionState::Running,
        started_at_ms: 1,
        ended_at_ms: None,
    };
    let operation = Operation {
        id: "crashed_operation".into(),
        session_id: session.id.clone(),
        run_id: run.id.clone(),
        call_id: "crashed_call".into(),
        name: "run_command".into(),
        arguments_hash: "fixture-hash".into(),
        arguments: json!({"command":"echo changed > unsafe-replay.txt"}),
        requires_approval: true,
        state: "started".into(),
        preview: json!({"command":"echo changed > unsafe-replay.txt"}),
    };
    connection.execute("UPDATE sessions SET value=?2, transcript=?3 WHERE id=?1", rusqlite::params![session.id,serde_json::to_string(&session).unwrap(),json!([{"type":"function_call","call_id":"crashed_call","name":"run_command","arguments":"{\"command\":\"echo changed > unsafe-replay.txt\"}"}]).to_string()]).unwrap();
    connection.execute("INSERT INTO runs(id,session_id,command_id,fingerprint,value) VALUES(?1,?2,?3,'fixture',?4)", rusqlite::params![run.id,run.session_id,run.command_id,serde_json::to_string(&run).unwrap()]).unwrap();
    connection.execute("INSERT INTO operations(id,session_id,run_id,state,value) VALUES(?1,?2,?3,'started',?4)", rusqlite::params![operation.id,operation.session_id,operation.run_id,serde_json::to_string(&operation).unwrap()]).unwrap();
    drop(connection);
    let identity = gateway
        .authenticate(&PresentedCredentials {
            x_api_key: Some(CLIENT_KEY.into()),
            ..Default::default()
        })
        .unwrap();
    let engine = AppEngine::open(
        gateway.clone(),
        identity,
        EngineOptions::new(temp.path().join("state")),
    )
    .unwrap();
    let recovered = engine.session(&session.id).unwrap().session;
    assert_eq!(recovered.state, SessionState::RecoveryRequired);
    assert!(
        engine
            .submit_turn(&session.id, "too-early", "Continue")
            .is_err()
    );
    assert!(
        engine
            .acknowledge_recovery(&session.id, recovered.revision, " ")
            .is_err()
    );
    assert!(
        engine
            .acknowledge_recovery(&session.id, recovered.revision, &"x".repeat(2001))
            .is_err()
    );
    assert!(
        engine
            .acknowledge_recovery(
                &session.id,
                recovered.revision + 1,
                "I reviewed the project"
            )
            .is_err()
    );
    let acknowledged = engine
        .acknowledge_recovery(
            &session.id,
            recovered.revision,
            "I reviewed the project and want a fresh explanation",
        )
        .unwrap();
    assert_eq!(acknowledged.state, SessionState::Interrupted);
    assert_eq!(engine.status().active_runs, 0);
    assert!(fake.requests().is_empty());
    assert!(!temp.path().join("project/unsafe-replay.txt").exists());
    fake.script(
        UPSTREAM_KEY,
        [Behaviour::text(
            "The previous command outcome is unknown; it was not repeated.",
        )],
    );
    engine
        .submit_turn(
            &session.id,
            "reviewed-next-turn",
            "Explain the current evidence; do not rerun the command",
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while engine.session(&session.id).unwrap().session.state != SessionState::Completed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!temp.path().join("project/unsafe-replay.txt").exists());
    assert!(
        fake.last()
            .body
            .to_string()
            .contains("Do not assume success")
    );
    engine.shutdown().await.unwrap();
    gateway.shutdown().await;
}
