use serde_json::json;
use std::{fs, time::Duration};
use switchyard_agent_tools::{definitions, execute, prepare, sha256};
use tokio_util::sync::CancellationToken;

#[test]
fn definitions_and_strict_argument_validation() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(definitions().len(), 4);
    for definition in definitions() {
        assert_eq!(definition["type"], "function");
        assert_eq!(definition["parameters"]["additionalProperties"], false);
    }
    assert!(
        prepare(
            directory.path(),
            "run_command",
            &json!({"command":"echo ok","network":true})
        )
        .is_err()
    );
    assert!(
        prepare(
            directory.path(),
            "run_command",
            &json!({"command":"echo ok","timeout_ms":300001})
        )
        .is_err()
    );
    assert!(
        prepare(
            directory.path(),
            "apply_patch",
            &json!({"edits":[{"path":"new.txt","content":"hello"}]})
        )
        .is_err()
    );
    assert!(prepare(directory.path(), "unknown", &json!({})).is_err());
}

#[test]
fn blocks_traversal_metadata_secrets_and_ambiguous_names() {
    let directory = tempfile::tempdir().unwrap();
    for path in [
        "../outside",
        "/outside",
        ".git/config",
        ".env",
        "nested/.env.local",
        "key.pem",
        ".ssh/id_rsa",
        ".codex/auth.json",
        "switchyard.toml",
        "sessions.sqlite3-wal",
        "CON.txt",
        "file.txt:stream",
        "nested/trailing.",
    ] {
        assert!(
            prepare(directory.path(), "read_file", &json!({"path":path})).is_err(),
            "allowed {path}"
        );
    }
    assert!(
        prepare(
            directory.path(),
            "run_command",
            &json!({"command":"echo ok","cwd":".."})
        )
        .is_err()
    );
}

#[tokio::test]
async fn environment_template_can_be_read_without_opening_real_env_files() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join(".env.example"),
        "API_URL=https://example.invalid\n",
    )
    .unwrap();
    let prepared = prepare(
        directory.path(),
        "read_file",
        &json!({"path":".env.example"}),
    )
    .unwrap();
    assert_eq!(
        execute(&prepared, CancellationToken::new()).await.status,
        "completed"
    );
    assert!(prepare(directory.path(), "read_file", &json!({"path":".env.local"})).is_err());
}

#[tokio::test]
async fn read_reports_hash_only_for_complete_content_and_bounds_utf8() {
    let directory = tempfile::tempdir().unwrap();
    let text = "aéz";
    fs::write(directory.path().join("test.txt"), text).unwrap();
    let full = prepare(directory.path(), "read_file", &json!({"path":"test.txt"})).unwrap();
    assert!(!full.requires_approval);
    let full = execute(&full, CancellationToken::new()).await;
    assert_eq!(full.status, "completed");
    assert_eq!(full.output["sha256"], sha256(text.as_bytes()));
    let partial = prepare(
        directory.path(),
        "read_file",
        &json!({"path":"test.txt","max_bytes":2}),
    )
    .unwrap();
    let partial = execute(&partial, CancellationToken::new()).await;
    assert_eq!(partial.status, "completed");
    assert_eq!(partial.output["content"], "a");
    assert!(partial.output["sha256"].is_null());
    assert!(partial.truncated);
}

#[tokio::test]
async fn search_excludes_secrets_build_output_and_supports_literal_queries() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::create_dir(directory.path().join("node_modules")).unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    fs::write(directory.path().join("src/main.rs"), "literal [needle]\n").unwrap();
    fs::write(directory.path().join(".env"), "secret [needle]\n").unwrap();
    fs::write(
        directory.path().join("node_modules/dep.js"),
        "ignored [needle]\n",
    )
    .unwrap();
    fs::write(directory.path().join(".git/config"), "private [needle]\n").unwrap();
    let plan = prepare(
        directory.path(),
        "search_files",
        &json!({"query":"[needle]"}),
    )
    .unwrap();
    let result = execute(&plan, CancellationToken::new()).await;
    assert_eq!(result.status, "completed");
    let matches = result.output["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["path"], "src/main.rs");
    assert_eq!(matches[0]["line"], 1);
}

#[tokio::test]
async fn approved_patch_preview_is_exact_and_stale_batch_does_not_clobber() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.txt"), "one\n").unwrap();
    fs::write(directory.path().join("b.txt"), "two\n").unwrap();
    let plan = prepare(
        directory.path(),
        "apply_patch",
        &json!({"edits":[
            {"path":"a.txt","before_sha256":sha256(b"one\n"),"content":"changed one\n"},
            {"path":"b.txt","before_sha256":sha256(b"two\n"),"content":"changed two\n"}
        ]}),
    )
    .unwrap();
    assert!(plan.requires_approval);
    assert_eq!(
        plan.preview["edits"][0]["diff"],
        "--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-one\n+changed one\n"
    );
    fs::write(directory.path().join("b.txt"), "user change\n").unwrap();
    let result = execute(&plan, CancellationToken::new()).await;
    assert_eq!(result.status, "failed");
    assert!(result.changes.is_empty());
    assert_eq!(
        fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("b.txt")).unwrap(),
        "user change\n"
    );
}

#[tokio::test]
async fn patch_creates_and_replaces_with_explicit_preconditions() {
    let directory = tempfile::tempdir().unwrap();
    let plan = prepare(
        directory.path(),
        "apply_patch",
        &json!({"edits":[{"path":"new.txt","before_sha256":null,"content":"new"}]}),
    )
    .unwrap();
    assert!(
        plan.preview["edits"][0]["diff"]
            .as_str()
            .unwrap()
            .contains("No newline at end of file")
    );
    let result = execute(&plan, CancellationToken::new()).await;
    assert_eq!(result.status, "completed", "{result:?}");
    assert_eq!(result.changes.len(), 1);
    assert_eq!(result.changes[0].after_sha256, sha256(b"new"));
    assert_eq!(
        fs::read_to_string(directory.path().join("new.txt")).unwrap(),
        "new"
    );
    let again = execute(&plan, CancellationToken::new()).await;
    assert_eq!(again.status, "failed");
    let replacement = prepare(directory.path(), "apply_patch", &json!({"edits":[{"path":"new.txt","before_sha256":sha256(b"new"),"content":"replacement\n"}]})).unwrap();
    let result = execute(&replacement, CancellationToken::new()).await;
    assert_eq!(result.status, "completed", "{result:?}");
    assert_eq!(
        fs::read_to_string(directory.path().join("new.txt")).unwrap(),
        "replacement\n"
    );
}

#[tokio::test]
async fn changed_public_metadata_cannot_change_a_prepared_operation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("test.txt"), "safe").unwrap();
    let mut plan = prepare(directory.path(), "read_file", &json!({"path":"test.txt"})).unwrap();
    plan.args["path"] = json!("../escape");
    assert_eq!(
        execute(&plan, CancellationToken::new()).await.status,
        "failed"
    );
    let mut patch = prepare(
        directory.path(),
        "apply_patch",
        &json!({"edits":[{"path":"new.txt","before_sha256":null,"content":"new"}]}),
    )
    .unwrap();
    patch.requires_approval = false;
    assert_eq!(
        execute(&patch, CancellationToken::new()).await.status,
        "failed"
    );
    assert!(!directory.path().join("new.txt").exists());
}

#[tokio::test]
async fn pre_cancelled_operations_do_not_modify_files_or_run_commands() {
    let directory = tempfile::tempdir().unwrap();
    let plan = prepare(
        directory.path(),
        "apply_patch",
        &json!({"edits":[{"path":"new.txt","before_sha256":null,"content":"new"}]}),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = execute(&plan, cancellation.clone()).await;
    assert_eq!(result.status, "cancelled");
    assert!(!directory.path().join("new.txt").exists());
    let plan = prepare(
        directory.path(),
        "run_command",
        &json!({"command":"echo should-not-run"}),
    )
    .unwrap();
    assert!(plan.requires_approval);
    assert_eq!(plan.preview["sandboxed"], false);
    assert_eq!(execute(&plan, cancellation).await.status, "cancelled");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_files_and_directories_never_escape_the_project() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("private.txt"), "needle outside").unwrap();
    symlink(outside.path(), directory.path().join("escape")).unwrap();
    symlink(
        outside.path().join("private.txt"),
        directory.path().join("file.txt"),
    )
    .unwrap();
    for path in ["escape/private.txt", "file.txt"] {
        assert!(prepare(directory.path(), "read_file", &json!({"path":path})).is_err());
        assert!(
            prepare(
                directory.path(),
                "apply_patch",
                &json!({"edits":[{"path":path,"before_sha256":null,"content":"bad"}]})
            )
            .is_err()
        );
    }
    let search = prepare(directory.path(), "search_files", &json!({"query":"needle"})).unwrap();
    assert_eq!(
        execute(&search, CancellationToken::new()).await.output["matches"],
        json!([])
    );
}

#[cfg(windows)]
#[tokio::test]
async fn junction_directories_are_blocked() {
    use std::os::windows::process::CommandExt;
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("private.txt"), "needle outside").unwrap();
    // Junctions do not require developer mode or symbolic-link privilege.
    let junction = directory.path().join("escape");
    let command = format!(
        "mklink /J \"{}\" \"{}\"",
        junction.display(),
        outside.path().display()
    );
    let status = std::process::Command::new("cmd.exe")
        .args(["/d", "/c"])
        // This string is already cmd.exe syntax. Normal Rust argument quoting
        // would backslash-escape its quotes, which cmd does not decode.
        .raw_arg(&command)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        prepare(
            directory.path(),
            "read_file",
            &json!({"path":"escape/private.txt"})
        )
        .is_err()
    );
    let search = prepare(directory.path(), "search_files", &json!({"query":"needle"})).unwrap();
    assert_eq!(
        execute(&search, CancellationToken::new()).await.output["matches"],
        json!([])
    );
    fs::remove_dir(junction).unwrap();
}

#[tokio::test]
async fn command_success_failure_timeout_and_output_bound() {
    let directory = tempfile::tempdir().unwrap();
    let plan = prepare(
        directory.path(),
        "run_command",
        &json!({"command":"echo hello"}),
    )
    .unwrap();
    let result = execute(&plan, CancellationToken::new()).await;
    assert_eq!(result.status, "completed", "{result:?}");
    assert!(result.output["stdout"].as_str().unwrap().contains("hello"));
    let failure = prepare(
        directory.path(),
        "run_command",
        &json!({"command":"exit 7"}),
    )
    .unwrap();
    let result = execute(&failure, CancellationToken::new()).await;
    assert_eq!(result.status, "failed");
    assert_eq!(result.exit_code, Some(7));
    let sleeper = if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let timeout = prepare(
        directory.path(),
        "run_command",
        &json!({"command":sleeper,"timeout_ms":200}),
    )
    .unwrap();
    assert_eq!(
        execute(&timeout, CancellationToken::new()).await.status,
        "timed_out"
    );
    let loud = if cfg!(windows) {
        "[Console]::Write(('x' * 200000))"
    } else {
        "head -c 200000 /dev/zero | tr '\\000' x"
    };
    let plan = prepare(directory.path(), "run_command", &json!({"command":loud})).unwrap();
    let result = execute(&plan, CancellationToken::new()).await;
    assert_eq!(result.status, "completed", "{result:?}");
    assert!(result.truncated);
    assert_eq!(result.output["stdout"].as_str().unwrap().len(), 128 * 1024);
}

#[tokio::test]
async fn cancelling_running_command_terminates_ordinary_descendants() {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let command = "$child = Start-Process -FilePath $PSHOME\\powershell.exe -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 60'; [IO.File]::WriteAllText((Join-Path (Get-Location) 'child.pid'), [string]$child.Id); Start-Sleep -Seconds 60";
    #[cfg(unix)]
    let command = "sleep 60 & child=$!; printf '%s' \"$child\" > child.pid; wait";
    let plan = prepare(
        directory.path(),
        "run_command",
        &json!({"command":command,"timeout_ms":60000}),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let task = tokio::spawn(async move { execute(&plan, task_cancel).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let pid_file = directory.path().join("child.pid");
    while !pid_file.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "child PID was never published"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, "cancelled", "{result:?}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        // SAFETY: OpenProcess is read-only synchronization access to a known child PID.
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !process.is_null() {
            // SAFETY: the process handle is valid and owned here.
            assert_eq!(unsafe { WaitForSingleObject(process, 5000) }, 0);
            unsafe {
                CloseHandle(process);
            }
        }
    }
    #[cfg(unix)]
    {
        // A killed child can briefly remain a zombie awaiting reaping; /proc's Z
        // state is terminated and cannot execute. macOS uses kill(pid, 0) polling.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 checks existence without sending a signal.
            let gone = unsafe { libc::kill(pid as i32, 0) } != 0;
            #[cfg(target_os = "linux")]
            let zombie = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
                s.split_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z'))
            });
            #[cfg(not(target_os = "linux"))]
            let zombie = false;
            if gone || zombie {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "child process survived cancellation"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
