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
    assert!(events.iter().any(|e| e.kind == "assistant_delta"));
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
        permission_boundary: String::new(),
    };
    let mut run = StoredRun {
        view,
        events: VecDeque::new(),
        event_bytes: 0,
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
