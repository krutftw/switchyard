//! A std-only test process, compiled by the crate's unit tests. No network/model API.
use std::io::{self, BufRead, Write};

fn send(line: &str) {
    println!("{line}");
    io::stdout().flush().unwrap();
}
fn completed(status: &str) {
    send(&format!(
        r#"{{"method":"turn/completed","params":{{"threadId":"thread-fixture","turn":{{"id":"turn-fixture","status":"{status}","items":[]}}}}}}"#
    ));
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("happy");
    if scenario == "descendant" {
        std::thread::sleep(std::time::Duration::from_secs(3));
        std::fs::write(args.get(2).unwrap(), "descendant survived").unwrap();
        return;
    }
    assert!(args.iter().any(|a| a == "sandbox_mode=\"read-only\""));
    assert!(args.iter().any(|a| a == "approval_policy=\"on-request\""));
    assert!(args.iter().any(|a| a == "approvals_reviewer=\"user\""));
    // Durable hosts ask Codex to save new threads; resume scenarios must never
    // start one.
    let durable = scenario.starts_with("durable");
    let resuming = scenario.starts_with("resume");
    eprintln!("fixture-private-token-should-never-appear-in-events");
    for input in io::stdin().lock().lines() {
        let input = input.unwrap();
        if input.contains("\"method\":\"initialize\"") {
            assert!(input.contains("\"experimentalApi\":false"));
            if let Ok(home) = std::env::var("CODEX_HOME") {
                let home = home.replace('\\', "\\\\").replace('"', "\\\"");
                send(&format!(
                    r#"{{"id":1,"result":{{"userAgent":"fixture","codexHome":"{home}"}}}}"#
                ));
            } else {
                send(r#"{"id":1,"result":{"userAgent":"fixture"}}"#);
            }
        } else if input.contains("\"method\":\"thread/resume\"") {
            assert!(resuming, "only resume scenarios may resume a thread");
            assert!(input.contains("\"threadId\":\"thread-fixture\""));
            assert!(input.contains("\"excludeTurns\":true"));
            assert!(input.contains("\"approvalPolicy\":\"on-request\""));
            assert!(input.contains("\"approvalsReviewer\":\"user\""));
            assert!(input.contains("\"sandbox\":\"read-only\""));
            let thread = if scenario == "resume_wrong_thread" {
                "thread-other"
            } else {
                "thread-fixture"
            };
            send(&format!(
                r#"{{"id":2,"result":{{"thread":{{"id":"{thread}"}},"model":"fixture-model","approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":{{"type":"readOnly","networkAccess":false}}}}}}"#
            ));
        } else if input.contains("\"method\":\"thread/start\"") {
            assert!(!resuming, "a continuation must resume the saved thread");
            assert!(input.contains("\"approvalPolicy\":\"on-request\""));
            assert!(input.contains("\"approvalsReviewer\":\"user\""));
            assert!(input.contains("\"sandbox\":\"read-only\""));
            assert!(input.contains(if durable {
                "\"ephemeral\":false"
            } else {
                "\"ephemeral\":true"
            }));
            if scenario == "policy_mismatch" {
                send(
                    r#"{"id":2,"result":{"thread":{"id":"thread-fixture"},"model":"fixture-model","approvalPolicy":"never","approvalsReviewer":"auto_review","sandbox":{"type":"dangerFullAccess"}}}"#,
                );
            } else {
                send(
                    r#"{"id":2,"result":{"thread":{"id":"thread-fixture"},"model":"fixture-model","approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":{"type":"readOnly","networkAccess":false}}}"#,
                );
            }
        } else if input.contains("\"method\":\"turn/start\"") {
            assert_ne!(
                scenario, "policy_mismatch",
                "model turn must not start after rejected policy"
            );
            assert_ne!(
                scenario, "resume_wrong_thread",
                "model turn must not start on an unconfirmed conversation"
            );
            assert!(
                input.contains("\"sandboxPolicy\":{\"type\":\"readOnly\",\"networkAccess\":false}")
            );
            assert!(input.contains("\"approvalPolicy\":\"on-request\""));
            assert!(input.contains("\"approvalsReviewer\":\"user\""));
            if scenario == "early_exit" {
                return;
            }
            send(
                r#"{"id":3,"result":{"turn":{"id":"turn-fixture","status":"inProgress","items":[]}}}"#,
            );
            send(
                r#"{"method":"turn/started","params":{"threadId":"thread-fixture","turn":{"id":"turn-fixture","status":"inProgress","items":[]}}}"#,
            );
            send(
                r#"{"method":"account/updated","params":{"threadId":"thread-fixture","token":"fixture-private-account-token"}}"#,
            );
            match scenario {
                "durable_complete" | "resume_complete" => {
                    for part in ["Saved ", "fixture ", "reply."] {
                        send(&format!(
                            r#"{{"method":"item/agentMessage/delta","params":{{"threadId":"thread-fixture","turnId":"turn-fixture","itemId":"message-1","delta":"{part}"}}}}"#
                        ));
                    }
                    send(
                        r#"{"method":"item/completed","params":{"threadId":"thread-fixture","turnId":"turn-fixture","item":{"id":"message-1","type":"agentMessage","text":"Saved fixture reply."}}}"#,
                    );
                    completed("completed");
                    return;
                }
                "malformed" => {
                    send("this is not JSON");
                    return;
                }
                "oversized" => {
                    send(&"x".repeat(300_000));
                    return;
                }
                "cancel" | "durable_cancel" => continue,
                "descendant_parent" => {
                    let marker = args.get(2).unwrap();
                    let _child = std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["descendant", marker])
                        .spawn()
                        .unwrap();
                    continue;
                }
                "unsupported" => {
                    send(
                        r#"{"id":71,"method":"item/permissions/requestApproval","params":{"threadId":"thread-fixture","turnId":"turn-fixture","permissions":{"network":{"enabled":true}}}}"#,
                    );
                    continue;
                }
                "file_no_preview" | "file_grant_root" | "file_preview" => {
                    if scenario != "file_no_preview" {
                        send(
                            r#"{"method":"item/started","params":{"threadId":"thread-fixture","turnId":"turn-fixture","item":{"id":"patch-1","type":"fileChange","status":"inProgress","changes":[{"path":"fixture.txt","kind":{"type":"update"},"diff":"-old\n+new"}]}}}"#,
                        );
                    }
                    if scenario == "file_grant_root" {
                        send(
                            r#"{"id":72,"method":"item/fileChange/requestApproval","params":{"threadId":"thread-fixture","turnId":"turn-fixture","itemId":"patch-1","startedAtMs":1,"grantRoot":"/"}}"#,
                        );
                    } else {
                        send(
                            r#"{"id":72,"method":"item/fileChange/requestApproval","params":{"threadId":"thread-fixture","turnId":"turn-fixture","itemId":"patch-1","startedAtMs":1,"grantRoot":null}}"#,
                        );
                    }
                    continue;
                }
                _ => {}
            }
            send(
                r#"{"method":"item/agentMessage/delta","params":{"threadId":"thread-fixture","turnId":"turn-fixture","itemId":"message-1","delta":"Fixture response."}}"#,
            );
            send(
                r#"{"id":70,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-fixture","turnId":"turn-fixture","itemId":"command-1","startedAtMs":1,"command":"echo fixture","cwd":"/fixture","reason":"Test operation"}}"#,
            );
            if scenario == "resolved" {
                send(
                    r#"{"method":"serverRequest/resolved","params":{"threadId":"thread-fixture","requestId":70}}"#,
                );
                completed("completed");
                return;
            }
            if scenario == "duplicate_approval" {
                send(
                    r#"{"id":70,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-fixture","turnId":"turn-fixture","itemId":"command-1","startedAtMs":1,"command":"echo CHANGED","cwd":"/fixture"}}"#,
                );
            }
        } else if input.contains("\"id\":70") || input.contains("\"id\":72") {
            assert!(
                input.contains("\"decision\":\"accept\"")
                    || input.contains("\"decision\":\"decline\"")
                    || input.contains("\"decision\":\"cancel\"")
            );
            assert!(!input.contains("acceptForSession"));
            send(
                r#"{"method":"item/completed","params":{"threadId":"thread-fixture","turnId":"turn-fixture","item":{"id":"message-1","type":"agentMessage","text":"Fixture response."}}}"#,
            );
            completed("completed");
            return;
        } else if input.contains("\"id\":71") {
            assert!(input.contains("\"error\":{\"code\":-32601"));
            completed("completed");
            return;
        } else if input.contains("\"method\":\"turn/interrupt\"") {
            send(r#"{"id":4,"result":{}}"#);
            completed("interrupted");
            return;
        }
    }
}
