use super::*;
use std::{path::PathBuf, sync::OnceLock, time::Duration};

fn fixture() -> PathBuf {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap().keep();
            let binary = dir.join(if cfg!(windows) {
                "protocol-fixture.exe"
            } else {
                "protocol-fixture"
            });
            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/child.rs");
            let result = std::process::Command::new("rustc")
                .arg("--edition=2024")
                .arg(source)
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "fixture compile failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            binary
        })
        .clone()
}

fn start(manager: &AdapterManager, scenario: &str) -> (tempfile::TempDir, AdapterRun) {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();
    let run = manager
        .start_process(
            StartRequest {
                adapter_id: "codex".into(),
                project_path: project.clone(),
                prompt: "Inspect the fixture; literal $(never execute) `text`.".into(),
                command_id: uuid::Uuid::new_v4().to_string(),
                continue_run_id: None,
                profile: None,
            },
            project,
            fixture(),
            vec![scenario.into()],
        )
        .unwrap();
    (directory, run)
}

async fn wait_until(
    manager: &AdapterManager,
    id: &str,
    predicate: impl Fn(&AdapterRun) -> bool,
) -> AdapterRun {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let run = manager.run(id).unwrap();
            if predicate(&run) {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "fixture timed out: {:?}; events {:?}",
            manager.run(id),
            manager.events(id, 0, 200)
        )
    })
}

#[tokio::test]
async fn structured_run_hash_bound_approval_and_secret_filtered_events() {
    let manager = AdapterManager::new();
    let (_directory, run) = start(&manager, "happy");
    let pending = wait_until(&manager, &run.id, |r| r.state == RunState::AwaitingApproval).await;
    assert_eq!(pending.model.as_deref(), Some("fixture-model"));
    let approval = &pending.pending_approvals[0];
    assert!(approval.can_allow_once);
    assert!(matches!(
        manager
            .decide(
                &run.id,
                &approval.id,
                "wrong-hash",
                ApprovalDecision::AllowOnce
            )
            .await,
        Err(AdapterError::Conflict(_))
    ));
    assert_eq!(manager.run(&run.id).unwrap().pending_approvals.len(), 1);
    manager
        .decide(
            &run.id,
            &approval.id,
            &approval.expected_hash,
            ApprovalDecision::AllowOnce,
        )
        .await
        .unwrap();
    let complete = wait_until(&manager, &run.id, |r| !r.state.is_active()).await;
    assert_eq!(complete.state, RunState::Completed);
    assert!(complete.pending_approvals.is_empty());
    assert!(
        manager
            .decide(
                &run.id,
                &approval.id,
                &approval.expected_hash,
                ApprovalDecision::AllowOnce
            )
            .await
            .is_err()
    );
    let events = manager.events(&run.id, 0, 200).unwrap();
    // Streamed fragments are coalesced into the completed message text.
    assert!(!events.iter().any(|e| e.kind == "assistant_delta"));
    assert!(events.iter().any(|e| e.kind == "item_completed"
        && e.payload.pointer("/item/text").and_then(Value::as_str) == Some("Fixture response.")));
    let serialized = serde_json::to_string(&events).unwrap();
    assert!(!serialized.contains("fixture-private"));
    assert!(!serialized.contains("account/updated"));
    manager.shutdown().await;
}

#[tokio::test]
async fn policy_mismatch_stops_before_model_request() {
    let manager = AdapterManager::new();
    let (_directory, run) = start(&manager, "policy_mismatch");
    let stopped = wait_until(&manager, &run.id, |r| !r.state.is_active()).await;
    assert_eq!(stopped.state, RunState::Failed);
    assert!(stopped.turn_id.is_none());
    assert!(stopped.model.is_none());
    manager.shutdown().await;
}

#[tokio::test]
async fn interruption_is_confirmed_by_turn_completion() {
    let manager = AdapterManager::new();
    let (_directory, run) = start(&manager, "cancel");
    wait_until(&manager, &run.id, |r| r.state == RunState::Running).await;
    manager.interrupt(&run.id).await.unwrap();
    assert_eq!(
        wait_until(&manager, &run.id, |r| !r.state.is_active())
            .await
            .state,
        RunState::Interrupted
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn malformed_oversized_and_unconfirmed_exit_require_recovery() {
    for scenario in ["malformed", "oversized", "early_exit", "duplicate_approval"] {
        let manager = AdapterManager::new();
        let (_directory, run) = start(&manager, scenario);
        let stopped = wait_until(&manager, &run.id, |r| !r.state.is_active()).await;
        assert_eq!(
            stopped.state,
            RunState::RecoveryRequired,
            "scenario {scenario}"
        );
        assert!(stopped.pending_approvals.is_empty());
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn incomplete_or_session_wide_patch_request_is_deny_only() {
    for scenario in ["file_no_preview", "file_grant_root"] {
        let manager = AdapterManager::new();
        let (_directory, run) = start(&manager, scenario);
        let pending =
            wait_until(&manager, &run.id, |r| r.state == RunState::AwaitingApproval).await;
        let approval = &pending.pending_approvals[0];
        assert!(!approval.can_allow_once);
        assert!(
            manager
                .decide(
                    &run.id,
                    &approval.id,
                    &approval.expected_hash,
                    ApprovalDecision::AllowOnce
                )
                .await
                .is_err()
        );
        manager
            .decide(
                &run.id,
                &approval.id,
                &approval.expected_hash,
                ApprovalDecision::Deny,
            )
            .await
            .unwrap();
        assert_eq!(
            wait_until(&manager, &run.id, |r| !r.state.is_active())
                .await
                .state,
            RunState::Completed
        );
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn exact_patch_preview_can_be_approved_once() {
    let manager = AdapterManager::new();
    let (_directory, run) = start(&manager, "file_preview");
    let pending = wait_until(&manager, &run.id, |r| r.state == RunState::AwaitingApproval).await;
    let approval = &pending.pending_approvals[0];
    assert!(approval.can_allow_once);
    assert_eq!(
        approval
            .preview
            .pointer("/item/changes/0/diff")
            .and_then(Value::as_str),
        Some("-old\n+new")
    );
    manager
        .decide(
            &run.id,
            &approval.id,
            &approval.expected_hash,
            ApprovalDecision::AllowOnce,
        )
        .await
        .unwrap();
    assert_eq!(
        wait_until(&manager, &run.id, |r| !r.state.is_active())
            .await
            .state,
        RunState::Completed
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn unsupported_permissions_are_refused_and_cli_resolutions_clear_approvals() {
    for scenario in ["unsupported", "resolved"] {
        let manager = AdapterManager::new();
        let (_directory, run) = start(&manager, scenario);
        let complete = wait_until(&manager, &run.id, |r| !r.state.is_active()).await;
        assert_eq!(complete.state, RunState::Completed);
        assert!(complete.pending_approvals.is_empty());
        if scenario == "resolved" {
            assert!(
                manager
                    .events(&run.id, 0, 200)
                    .unwrap()
                    .iter()
                    .any(|e| e.kind == "approval_resolved")
            );
        }
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn retries_are_idempotent_and_changed_requests_conflict() {
    let manager = AdapterManager::new();
    let (directory, run) = start(&manager, "cancel");
    let mut request = StartRequest {
        adapter_id: "codex".into(),
        project_path: directory.path().canonicalize().unwrap(),
        prompt: "Inspect the fixture; literal $(never execute) `text`.".into(),
        command_id: run.command_id.clone(),
        continue_run_id: None,
        profile: None,
    };
    let repeat = manager
        .start_process(
            request.clone(),
            request.project_path.clone(),
            fixture(),
            vec!["cancel".into()],
        )
        .unwrap();
    assert_eq!(repeat.id, run.id);
    request.profile = Some(ProfileBinding {
        id: "system-codex".into(),
        name: "Existing Codex CLI".into(),
        agent_id: "codex".into(),
        home: PathBuf::new(),
        managed: false,
    });
    assert!(matches!(
        manager.start_process(
            request.clone(),
            request.project_path.clone(),
            fixture(),
            vec!["cancel".into()]
        ),
        Err(AdapterError::Conflict(_))
    ));
    request.profile = None;
    request.prompt = "Different request".into();
    assert!(matches!(
        manager.start_process(
            request.clone(),
            request.project_path.clone(),
            fixture(),
            vec!["cancel".into()]
        ),
        Err(AdapterError::Conflict(_))
    ));
    assert_eq!(manager.list_runs().len(), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn retained_retry_uses_original_binding_without_cli_or_filesystem_readiness() {
    let manager = AdapterManager::new();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let profile_home = directory.path().join("profile");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&profile_home).unwrap();
    let project = project.canonicalize().unwrap();
    let binding = ProfileBinding {
        id: "fixture-account".into(),
        name: "Original account".into(),
        agent_id: "codex".into(),
        home: profile_home.canonicalize().unwrap(),
        managed: true,
    };
    let request = StartRequest {
        adapter_id: "codex".into(),
        project_path: project.clone(),
        prompt: "Fixture request".into(),
        command_id: uuid::Uuid::new_v4().to_string(),
        continue_run_id: None,
        profile: Some(binding.clone()),
    };
    let run = manager
        .start_process(request.clone(), project, fixture(), vec!["cancel".into()])
        .unwrap();
    wait_until(&manager, &run.id, |view| view.state == RunState::Running).await;
    manager.interrupt(&run.id).await.unwrap();
    manager.shutdown().await;
    // Delete only this fixture's owned temporary tree after its child is reaped.
    directory.close().unwrap();
    assert!(!request.project_path.exists());
    assert!(!binding.home.exists());
    assert_eq!(
        manager.retained_profile(&request.command_id),
        Some(binding.clone())
    );
    let repeated = manager.start(request.clone()).unwrap();
    assert_eq!(repeated.id, run.id);
    assert_eq!(repeated.profile_id.as_deref(), Some(binding.id.as_str()));
    assert_eq!(repeated.profile_name.as_deref(), Some("Original account"));
    assert_eq!(
        manager.retry_existing(&request).unwrap().unwrap().id,
        run.id
    );
    let mut changed = request.clone();
    changed.prompt.push_str(" changed");
    assert!(matches!(
        manager.start(changed),
        Err(AdapterError::Conflict(_))
    ));
    let mut changed = request.clone();
    changed.profile.as_mut().unwrap().home = PathBuf::from("different-home");
    assert!(matches!(
        manager.start(changed),
        Err(AdapterError::Conflict(_))
    ));
    let mut changed = request;
    changed.command_id = uuid::Uuid::new_v4().to_string();
    assert!(manager.retry_existing(&changed).unwrap().is_none());
    assert_eq!(manager.list_runs().len(), 1);
}

#[tokio::test]
async fn shutdown_stops_descendants_and_marks_uncertain_runs() {
    let manager = AdapterManager::new();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();
    let marker = directory.path().join("survivor.txt");
    let request = StartRequest {
        adapter_id: "codex".into(),
        project_path: project.clone(),
        prompt: "fixture".into(),
        command_id: uuid::Uuid::new_v4().to_string(),
        continue_run_id: None,
        profile: None,
    };
    let run = manager
        .start_process(
            request,
            project,
            fixture(),
            vec![
                "descendant_parent".into(),
                marker.to_string_lossy().into_owned(),
            ],
        )
        .unwrap();
    wait_until(&manager, &run.id, |r| r.state == RunState::Running).await;
    // Give the fixture child time to create its descendant, then close the host.
    tokio::time::sleep(Duration::from_millis(200)).await;
    manager.shutdown().await;
    assert_eq!(
        manager.run(&run.id).unwrap().state,
        RunState::RecoveryRequired
    );
    tokio::time::sleep(Duration::from_millis(3300)).await;
    assert!(
        !marker.exists(),
        "the owned process descendant survived host shutdown"
    );
}

#[test]
fn event_retention_and_cursor_limits_are_explicit() {
    let view = AdapterRun {
        id: "fixture".into(),
        adapter_id: "codex".into(),
        command_id: uuid::Uuid::new_v4().to_string(),
        conversation_id: "fixture".into(),
        continued_from: None,
        title: String::new(),
        profile_id: None,
        profile_name: None,
        project_path: "/fixture".into(),
        state: RunState::Running,
        thread_id: None,
        turn_id: None,
        model: None,
        started_at_ms: 0,
        ended_at_ms: None,
        last_seq: 0,
        first_retained_seq: 1,
        pending_approvals: Vec::new(),
        ephemeral: true,
        history_error: None,
        permission_boundary: String::new(),
    };
    let mut run = StoredRun {
        view,
        events: VecDeque::new(),
        event_bytes: 0,
        store: None,
    };
    for i in 0..600 {
        run.event("notice", json!({"text":"x".repeat(5000),"i":i}));
    }
    assert!(run.events.len() <= MAX_EVENTS);
    assert!(run.event_bytes <= MAX_EVENT_BYTES);
    assert_eq!(run.view.last_seq, 600);
    assert!(run.view.first_retained_seq > 1);
    assert_eq!(run.events.back().unwrap().seq, 600);
}

// ---- Durable conversations ------------------------------------------------

fn durable_request(project: &std::path::Path, continue_run_id: Option<&str>) -> StartRequest {
    StartRequest {
        adapter_id: "codex".into(),
        project_path: project.to_path_buf(),
        prompt: "Fixture request".into(),
        command_id: uuid::Uuid::new_v4().to_string(),
        continue_run_id: continue_run_id.map(Into::into),
        profile: None,
    }
}

fn launch(manager: &AdapterManager, request: &StartRequest, scenario: &str) -> Result<AdapterRun> {
    manager.start_process(
        request.clone(),
        request.project_path.clone(),
        fixture(),
        vec![scenario.into()],
    )
}

async fn finished(manager: &AdapterManager, id: &str) -> AdapterRun {
    let run = wait_until(manager, id, |r| !r.state.is_active()).await;
    // Wait for process cleanup too; continuation requires it.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if manager
                .inner
                .runs
                .lock()
                .get(id)
                .unwrap()
                .process_finished()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    manager.run(id).unwrap_or(run)
}

fn all_events(manager: &AdapterManager, id: &str) -> Vec<AdapterEvent> {
    manager.events(id, 0, 200).unwrap()
}

#[tokio::test]
async fn durable_conversation_survives_restart_and_resumes_the_saved_thread() {
    let data = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();

    let first_manager = AdapterManager::open(data.path()).unwrap();
    assert!(first_manager.is_durable());
    let first_request = durable_request(&project, None);
    let first = launch(&first_manager, &first_request, "durable_complete").unwrap();
    assert!(!first.ephemeral);
    assert_eq!(first.conversation_id, first.id);
    let first = finished(&first_manager, &first.id).await;
    assert_eq!(first.state, RunState::Completed);
    assert_eq!(first.thread_id.as_deref(), Some("thread-fixture"));
    // Streamed fragments are coalesced into the completed message.
    let events = all_events(&first_manager, &first.id);
    assert!(!events.iter().any(|e| e.kind == "assistant_delta"));
    first_manager.shutdown().await;
    drop(first_manager);

    let second_manager = AdapterManager::open(data.path()).unwrap();
    let restored = second_manager.run(&first.id).unwrap();
    assert_eq!(restored.state, RunState::Completed);
    assert_eq!(restored.thread_id.as_deref(), Some("thread-fixture"));
    let events = all_events(&second_manager, &first.id);
    assert!(
        events
            .iter()
            .any(|e| e.kind == "user_task" && e.payload["text"] == "Fixture request")
    );
    assert!(events.iter().any(|e| e.kind == "item_completed"
        && e.payload.pointer("/item/text").and_then(Value::as_str)
            == Some("Saved fixture reply.")));
    // A retry of the accepted request returns the saved run without a process.
    assert_eq!(
        launch(&second_manager, &first_request, "durable_complete")
            .unwrap()
            .id,
        first.id
    );

    let continue_request = durable_request(&project, Some(&first.id));
    let second = launch(&second_manager, &continue_request, "resume_complete").unwrap();
    assert_eq!(second.conversation_id, first.id);
    assert_eq!(second.continued_from.as_deref(), Some(first.id.as_str()));
    let second = finished(&second_manager, &second.id).await;
    assert_eq!(second.state, RunState::Completed);
    assert_eq!(second.thread_id.as_deref(), Some("thread-fixture"));
    let events = all_events(&second_manager, &second.id);
    assert!(
        events
            .iter()
            .any(|e| e.kind == "user_task" && e.payload["continued"] == true)
    );
    assert!(events.iter().any(|e| {
        e.kind == "notice"
            && e.payload["message"]
                .as_str()
                .is_some_and(|m| m.contains("resumed the saved conversation"))
    }));
    // Only the latest run of a conversation can be continued.
    assert!(matches!(
        launch(
            &second_manager,
            &durable_request(&project, Some(&first.id)),
            "resume_complete"
        ),
        Err(AdapterError::Conflict(_))
    ));
    assert_eq!(second_manager.list_runs()[0].id, second.id);
    second_manager.shutdown().await;
}

#[tokio::test]
async fn host_stop_during_a_run_requires_review_before_continuing() {
    let data = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();
    let manager = AdapterManager::open(data.path()).unwrap();
    let run = launch(&manager, &durable_request(&project, None), "durable_cancel").unwrap();
    wait_until(&manager, &run.id, |r| r.state == RunState::Running).await;
    manager.shutdown().await;
    drop(manager);

    let manager = AdapterManager::open(data.path()).unwrap();
    assert_eq!(
        manager.run(&run.id).unwrap().state,
        RunState::RecoveryRequired
    );
    assert!(matches!(
        launch(
            &manager,
            &durable_request(&project, Some(&run.id)),
            "resume_complete"
        ),
        Err(AdapterError::Conflict(_))
    ));
    let acknowledged = manager.acknowledge_recovery(&run.id).unwrap();
    assert_eq!(acknowledged.state, RunState::Interrupted);
    assert!(manager.acknowledge_recovery(&run.id).is_err());
    assert!(
        all_events(&manager, &run.id)
            .iter()
            .any(|e| e.kind == "recovery_acknowledged")
    );
    let next = launch(
        &manager,
        &durable_request(&project, Some(&run.id)),
        "resume_complete",
    )
    .unwrap();
    assert_eq!(
        finished(&manager, &next.id).await.state,
        RunState::Completed
    );
    manager.shutdown().await;
    drop(manager);
    // The acknowledgement itself was saved.
    let manager = AdapterManager::open(data.path()).unwrap();
    assert_eq!(manager.run(&run.id).unwrap().state, RunState::Interrupted);
}

#[tokio::test]
async fn runs_active_at_a_crash_are_marked_never_resumed() {
    let data = tempfile::tempdir().unwrap();
    let template = |id: &str, state: RunState, thread: Option<&str>| AdapterRun {
        id: id.into(),
        adapter_id: "codex".into(),
        command_id: uuid::Uuid::new_v4().to_string(),
        conversation_id: id.into(),
        continued_from: None,
        title: String::new(),
        profile_id: None,
        profile_name: None,
        project_path: "/fixture".into(),
        state,
        thread_id: thread.map(Into::into),
        turn_id: None,
        model: None,
        started_at_ms: 1,
        ended_at_ms: None,
        last_seq: 0,
        first_retained_seq: 1,
        pending_approvals: Vec::new(),
        ephemeral: false,
        history_error: None,
        permission_boundary: String::new(),
    };
    {
        // Simulate a host that died without recording a final state.
        let store = store::Store::open(data.path()).unwrap();
        store
            .insert_run(
                &template(
                    "adapter_running",
                    RunState::AwaitingApproval,
                    Some("thread-a"),
                ),
                "h1",
                None,
                &[],
            )
            .unwrap();
        store
            .insert_run(
                &template("adapter_starting", RunState::Starting, None),
                "h2",
                None,
                &[],
            )
            .unwrap();
    }
    let manager = AdapterManager::open(data.path()).unwrap();
    let uncertain = manager.run("adapter_running").unwrap();
    assert_eq!(uncertain.state, RunState::RecoveryRequired);
    assert!(uncertain.pending_approvals.is_empty());
    assert!(uncertain.ended_at_ms.is_some());
    assert!(all_events(&manager, "adapter_running").iter().any(|e| {
        e.kind == "adapter_error"
            && e.payload["message"]
                .as_str()
                .is_some_and(|m| m.contains("Nothing was replayed"))
    }));
    assert_eq!(
        manager.run("adapter_starting").unwrap().state,
        RunState::Failed
    );
    // Recovery control is not available for runs this host did not start.
    assert!(matches!(
        manager.interrupt("adapter_running").await,
        Err(AdapterError::Conflict(_))
    ));
}

#[tokio::test]
async fn continuation_requires_the_same_project_account_and_a_saved_thread() {
    // In-memory hosts do not save threads, so their runs cannot be continued.
    let memory = AdapterManager::new();
    let (directory, run) = start(&memory, "resolved");
    let run = finished(&memory, &run.id).await;
    assert!(run.ephemeral);
    let project = directory.path().canonicalize().unwrap();
    assert!(matches!(
        launch(
            &memory,
            &durable_request(&project, Some(&run.id)),
            "resolved"
        ),
        Err(AdapterError::Conflict(_))
    ));
    memory.shutdown().await;

    let data = tempfile::tempdir().unwrap();
    let manager = AdapterManager::open(data.path()).unwrap();
    let first = launch(
        &manager,
        &durable_request(&project, None),
        "durable_complete",
    )
    .unwrap();
    let first = finished(&manager, &first.id).await;
    assert!(matches!(
        launch(
            &manager,
            &durable_request(&project, Some("adapter_missing")),
            "resume_complete"
        ),
        Err(AdapterError::NotFound(_))
    ));
    let mut other_account = durable_request(&project, Some(&first.id));
    other_account.profile = Some(ProfileBinding {
        id: "system-codex".into(),
        name: "Existing Codex CLI".into(),
        agent_id: "codex".into(),
        home: PathBuf::new(),
        managed: false,
    });
    assert!(matches!(
        launch(&manager, &other_account, "resume_complete"),
        Err(AdapterError::Conflict(_))
    ));
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere = elsewhere.path().canonicalize().unwrap();
    assert!(matches!(
        launch(
            &manager,
            &durable_request(&elsewhere, Some(&first.id)),
            "resume_complete"
        ),
        Err(AdapterError::Conflict(_))
    ));
    // Codex resuming a different thread stops before any turn is sent, and that
    // failed attempt does not block continuing from the real latest run.
    let wrong = launch(
        &manager,
        &durable_request(&project, Some(&first.id)),
        "resume_wrong_thread",
    )
    .unwrap();
    let wrong = finished(&manager, &wrong.id).await;
    assert_eq!(wrong.state, RunState::Failed);
    assert!(wrong.thread_id.is_none() && wrong.turn_id.is_none());
    let next = launch(
        &manager,
        &durable_request(&project, Some(&first.id)),
        "resume_complete",
    )
    .unwrap();
    assert_eq!(
        finished(&manager, &next.id).await.state,
        RunState::Completed
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn a_thread_without_a_confirmed_turn_cannot_be_continued() {
    let data = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();
    let mut view = fixture_view("adapter_turnless", "adapter_turnless", 1);
    view.state = RunState::Failed;
    view.thread_id = Some("thread-fixture".into());
    view.ephemeral = false;
    view.project_path = project.to_string_lossy().into_owned();
    {
        let store = store::Store::open(data.path()).unwrap();
        store.insert_run(&view, "hash", None, &[]).unwrap();
    }
    let manager = AdapterManager::open(data.path()).unwrap();
    let result = launch(
        &manager,
        &durable_request(&project, Some("adapter_turnless")),
        "resume_complete",
    );
    assert!(
        matches!(&result, Err(AdapterError::Conflict(message)) if message.contains("never confirmed a message"))
    );
    assert_eq!(manager.list_runs().len(), 1);
}

#[test]
fn a_second_host_cannot_open_live_run_history() {
    let data = tempfile::tempdir().unwrap();
    let _first = AdapterManager::open(data.path()).unwrap();
    assert!(matches!(
        AdapterManager::open(data.path()),
        Err(AdapterError::Unavailable(_))
    ));
}

fn fixture_view(id: &str, conversation: &str, started_at_ms: u64) -> AdapterRun {
    serde_json::from_value(json!({
        "id": id, "adapter_id": "codex", "command_id": uuid::Uuid::new_v4().to_string(),
        "conversation_id": conversation,
        "continued_from": null, "profile_id": null, "profile_name": null,
        "project_path": "/fixture", "state": "completed", "thread_id": null,
        "turn_id": null, "model": null, "started_at_ms": started_at_ms,
        "ended_at_ms": null, "last_seq": 0, "first_retained_seq": 1,
        "pending_approvals": [], "ephemeral": true, "permission_boundary": ""
    }))
    .unwrap()
}

#[test]
fn retention_removes_the_oldest_inactive_conversations_first() {
    let manager = AdapterManager::new();
    let pruned_command;
    {
        let mut runs = manager.inner.runs.lock();
        for i in 0..MAX_RUNS {
            let id = format!("adapter_{i:04}");
            let conversation = if i < 2 {
                "conversation_old".to_owned()
            } else {
                id.clone()
            };
            let mut view = fixture_view(&id, &conversation, 1000 + i as u64);
            if i == 5 {
                view.state = RunState::Running;
            }
            runs.insert(
                id,
                Entry {
                    request_hash: String::new(),
                    profile: None,
                    run: Arc::new(Mutex::new(StoredRun {
                        view,
                        events: VecDeque::new(),
                        event_bytes: 0,
                        store: None,
                    })),
                    control: None,
                    task: None,
                },
            );
        }
        pruned_command = runs["adapter_0000"].run.lock().view.command_id.clone();
        manager.prune_locked(&mut runs, "adapter_0002").unwrap();
        // The two-run oldest conversation is removed together; the kept and
        // active conversations remain.
        assert_eq!(runs.len(), MAX_RUNS - 2);
        assert!(!runs.contains_key("adapter_0000") && !runs.contains_key("adapter_0001"));
        assert!(runs.contains_key("adapter_0002") && runs.contains_key("adapter_0005"));
    }
    // A delayed retry of a removed run's command is refused, never started again.
    let project = tempfile::tempdir().unwrap();
    let result = manager.start_process(
        StartRequest {
            adapter_id: "codex".into(),
            project_path: project.path().to_path_buf(),
            prompt: "Retried after its run was removed".into(),
            command_id: pruned_command,
            continue_run_id: None,
            profile: None,
        },
        project.path().canonicalize().unwrap(),
        PathBuf::from("must-not-be-spawned"),
        Vec::new(),
    );
    assert!(matches!(result, Err(AdapterError::Conflict(_))));
    assert_eq!(manager.inner.runs.lock().len(), MAX_RUNS - 2);
}

#[test]
fn removed_command_ids_stay_retired_after_restart() {
    let data = tempfile::tempdir().unwrap();
    let view = fixture_view("adapter_old", "adapter_old", 1);
    let command = view.command_id.clone();
    {
        let store = store::Store::open(data.path()).unwrap();
        store.insert_run(&view, "hash", None, &[]).unwrap();
        store.delete_conversation("adapter_old").unwrap();
    }
    let manager = AdapterManager::open(data.path()).unwrap();
    assert!(manager.list_runs().is_empty());
    assert!(manager.inner.retired.lock().contains(&command));
}

/// Opt-in live check of the real continuation path against an installed Codex CLI.
///
/// Set `SWITCHYA_LIVE_CODEX_EXE` to the native `codex` executable and
/// `SWITCHYA_LIVE_CODEX_ROLLOUT` to an existing `rollout-*.jsonl`. The rollout is
/// copied into a fresh, signed-out managed Codex home that uses file credential
/// storage, so the turn cannot authenticate and no model usage is possible. The
/// user's own Codex home is only read. Run with
/// `cargo test -p switchyard-agent-adapters live_codex -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "requires an installed Codex CLI and an existing rollout file"]
async fn live_codex_resume_of_a_saved_thread_in_a_signed_out_home() {
    let (Ok(exe), Ok(rollout)) = (
        std::env::var("SWITCHYA_LIVE_CODEX_EXE"),
        std::env::var("SWITCHYA_LIVE_CODEX_ROLLOUT"),
    ) else {
        panic!("set SWITCHYA_LIVE_CODEX_EXE and SWITCHYA_LIVE_CODEX_ROLLOUT");
    };
    let rollout = PathBuf::from(rollout);
    let name = rollout.file_name().unwrap().to_string_lossy().into_owned();
    let thread = name
        .strip_suffix(".jsonl")
        .and_then(|stem| stem.get(stem.len().saturating_sub(36)..))
        .unwrap()
        .to_owned();
    assert!(
        uuid::Uuid::parse_str(&thread).is_ok(),
        "rollout name must end in a thread ID"
    );

    let home = tempfile::tempdir().unwrap();
    let sessions = home
        .path()
        .join("sessions")
        .join("2026")
        .join("01")
        .join("01");
    std::fs::create_dir_all(&sessions).unwrap();
    let copied = sessions.join(&name);
    std::fs::copy(&rollout, &copied).unwrap();
    let before = std::fs::metadata(&copied).unwrap().len();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap();
    let profile = ProfileBinding {
        id: "live-probe".into(),
        name: "Signed-out live probe".into(),
        agent_id: "codex".into(),
        home: home.path().canonicalize().unwrap(),
        managed: true,
    };

    let data = tempfile::tempdir().unwrap();
    let mut earlier = fixture_view("adapter_live_earlier", "adapter_live_earlier", 1);
    earlier.state = RunState::Completed;
    earlier.thread_id = Some(thread.clone());
    earlier.turn_id = Some("earlier-turn".into());
    earlier.ephemeral = false;
    earlier.project_path = project.to_string_lossy().into_owned();
    earlier.profile_id = Some(profile.id.clone());
    {
        let store = store::Store::open(data.path()).unwrap();
        store
            .insert_run(&earlier, "hash", Some(&profile), &[])
            .unwrap();
    }
    let manager = AdapterManager::open(data.path()).unwrap();
    let mut request = durable_request(&project, Some("adapter_live_earlier"));
    request.prompt = "Switchya live protocol check. Reply with OK.".into();
    request.profile = Some(profile);
    let run = manager
        .start_process(request, project.clone(), PathBuf::from(exe), Vec::new())
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut run = manager.run(&run.id).unwrap();
    while run.state.is_active() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        run = manager.run(&run.id).unwrap();
    }
    let still_active = run.state.is_active();
    if still_active {
        let _ = manager.interrupt(&run.id).await;
        tokio::time::sleep(Duration::from_secs(6)).await;
        run = manager.run(&run.id).unwrap();
    }
    let events = all_events(&manager, &run.id);
    println!("still_active_after_120s={still_active}");
    println!(
        "state={:?} thread={:?} turn={:?} model={:?}",
        run.state, run.thread_id, run.turn_id, run.model
    );
    for event in &events {
        let detail = event
            .payload
            .get("message")
            .or_else(|| event.payload.get("state"))
            .or_else(|| event.payload.get("status"))
            .cloned()
            .unwrap_or(Value::Null);
        println!("{} {} {}", event.seq, event.kind, detail);
    }
    let after = std::fs::metadata(&copied).unwrap().len();
    println!("rollout bytes before={before} after={after}");
    // Resume and the policy read-back succeeded: Switchya attaches the thread
    // only after Codex returned the same ID with the confirmed policy.
    assert_eq!(run.thread_id.as_deref(), Some(thread.as_str()));
    assert!(events.iter().any(|e| {
        e.kind == "notice"
            && e.payload["message"]
                .as_str()
                .is_some_and(|m| m.contains("resumed the saved conversation"))
    }));
    manager.shutdown().await;
}
