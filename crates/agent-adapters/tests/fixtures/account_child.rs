use std::io::{self, BufRead, Write};

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_default();
    if scenario == "claude_status" {
        println!(r#"{{"loggedIn":false,"secret":"fixture-do-not-forward"}}"#);
        return;
    }
    for name in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "NODE_OPTIONS",
    ] {
        assert!(
            std::env::var_os(name).is_none(),
            "managed profile inherited an authentication or runtime override"
        );
    }
    let home = std::env::var("CODEX_HOME").unwrap();
    let escaped = home.replace('\\', "\\\\").replace('"', "\\\"");
    let input = io::stdin();
    for line in input.lock().lines() {
        let line = line.unwrap();
        assert!(
            !line.contains("turn/start"),
            "account operation started a model turn"
        );
        if line.contains("\"method\":\"initialize\"") {
            if scenario == "oversized" {
                println!("{}", "x".repeat(300_000));
            } else if scenario == "malformed" {
                println!("not json");
            } else if scenario == "wrong_home" {
                println!(r#"{{"id":1,"result":{{"codexHome":"/not-the-profile"}}}}"#);
            } else {
                println!(r#"{{"id":1,"result":{{"codexHome":"{escaped}"}}}}"#);
            }
        } else if line.contains("account/read") {
            if scenario.starts_with("login_") {
                println!(r#"{{"id":2,"result":{{"account":null,"requiresOpenaiAuth":true}}}}"#);
            } else {
                println!(
                    r#"{{"id":2,"result":{{"account":{{"type":"chatgpt","email":"fixture@example.test","planType":"pro","token":"fixture-do-not-forward"}},"requiresOpenaiAuth":true}}}}"#
                );
            }
        } else if line.contains("account/rateLimits/read") {
            assert!(line.contains("\"excludeResetCreditDetails\":true"));
            println!(
                r#"{{"id":3,"result":{{"rateLimits":{{"primary":{{"usedPercent":73,"windowDurationMins":300,"resetsAt":12345}},"secondary":null}},"ordinaryUsageAllowed":null,"secret":"fixture-do-not-forward"}}}}"#
            );
        } else if line.contains("account/login/start") {
            assert!(scenario.starts_with("login_"));
            println!(
                r#"{{"id":3,"result":{{"type":"chatgpt","loginId":"fixture-login","authUrl":"https://auth.openai.com/oauth/authorize?state=fixture"}}}}"#
            );
            if scenario == "login_success" {
                println!(
                    r#"{{"method":"account/login/completed","params":{{"loginId":"fixture-login","success":true}}}}"#
                );
            }
        } else if line.contains("account/login/cancel") {
            println!(r#"{{"id":4,"result":{{"status":"canceled"}}}}"#);
        }
        io::stdout().flush().unwrap();
    }
}
