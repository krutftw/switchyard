//! Explicit user-requested terminal launch. No prompts, credentials, or shell flags
//! come from the project; native Claude retains its own interactive permissions.
use crate::{AdapterError, ProfileBinding, Result};
#[cfg(any(target_os = "linux", test))]
use std::path::PathBuf;
use std::{
    ffi::{OsStr, OsString},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, runtime::Handle};

// Fixed shell program, with the opened project and executable passed as data.
// This also protects paths containing '=' from env's assignment parser.
const RUN_IN_PROJECT: &str = "cd -- \"$1\" && shift && exec \"$@\"";

pub(crate) fn launch(executable: &Path, binding: &ProfileBinding, project: &Path) -> Result<()> {
    crate::profile::validate(binding, "claude")?;
    if !binding.managed || !native_executable(executable) {
        return Err(AdapterError::Unavailable(
            "A managed Claude profile and installed native Claude executable are required.".into(),
        ));
    }
    if !project.is_absolute()
        || !project.is_dir()
        || project.canonicalize().ok().as_deref() != Some(project)
    {
        return Err(AdapterError::Invalid(
            "Reopen the project before launching its terminal.".into(),
        ));
    }
    let runtime = Handle::try_current().map_err(|_| {
        AdapterError::Unavailable("The local host cannot monitor a terminal launch.".into())
    })?;
    let environment = environment(binding, std::env::vars_os());
    #[cfg(target_os = "macos")]
    let (mut command, must_exit) = (macos_command(executable, project, &environment)?, true);
    #[cfg(target_os = "linux")]
    let (mut command, must_exit) = (linux_command(executable, project, &environment)?, false);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (runtime, environment);
        return Err(AdapterError::Unavailable(
            "Opening an interactive terminal is not supported on this operating system.".into(),
        ));
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        command
            .current_dir(project)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|_| {
            AdapterError::Unavailable(
                "The terminal could not start. Check its installation.".into(),
            )
        })?;
        let deadline = Instant::now()
            + if must_exit {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(250)
            };
        loop {
            match child.try_wait() {
                Ok(Some(status)) if must_exit && status.success() => return Ok(()),
                Ok(Some(_)) => {
                    return Err(AdapterError::Unavailable(
                        "The terminal did not accept the launch request. Check its display and permissions.".into(),
                    ));
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) if !must_exit => {
                    // A user-owned interactive window outlives this request. Reap its
                    // launcher when it exits without terminating it on host shutdown.
                    runtime.spawn(async move {
                        let _ = child.wait().await;
                    });
                    return Ok(());
                }
                _ => {
                    let _ = child.start_kill();
                    runtime.spawn(async move {
                        let _ = child.wait().await;
                    });
                    return Err(AdapterError::Unavailable(
                        "Terminal launch was not confirmed. Check for an open window or permission prompt before retrying.".into(),
                    ));
                }
            }
        }
    }
}

fn native_executable(path: &Path) -> bool {
    path.is_absolute()
        && path
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn allowed_environment(key: &OsStr) -> bool {
    matches!(
        key.to_str(),
        Some(
            "HOME"
                | "USER"
                | "LOGNAME"
                | "PATH"
                | "LANG"
                | "LC_ALL"
                | "LC_CTYPE"
                | "LC_MESSAGES"
                | "LC_COLLATE"
                | "LC_NUMERIC"
                | "LC_TIME"
                | "TZ"
                | "TMPDIR"
                | "DISPLAY"
                | "WAYLAND_DISPLAY"
                | "XAUTHORITY"
                | "XDG_RUNTIME_DIR"
                | "XDG_SESSION_TYPE"
                | "XDG_CURRENT_DESKTOP"
                | "DBUS_SESSION_BUS_ADDRESS"
        )
    )
}

fn environment(
    binding: &ProfileBinding,
    ambient: impl Iterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut command = Command::new("/usr/bin/env");
    command.env_clear();
    for (key, value) in ambient.filter(|(key, _)| allowed_environment(key)) {
        if key == "PATH" {
            let paths = std::env::split_paths(&value).filter(|path| path.is_absolute());
            if let Ok(path) = std::env::join_paths(paths) {
                command.env(key, path);
            }
        } else {
            command.env(key, value);
        }
    }
    // Ensure native CLIs can find ordinary OS utilities without a project-relative PATH.
    if command
        .as_std()
        .get_envs()
        .all(|(key, value)| key != "PATH" || value.is_none_or(OsStr::is_empty))
    {
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    }
    command.env("TERM", "xterm-256color");
    crate::profile::configure(&mut command, Some(binding));
    command
        .as_std()
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_os_string(), value.to_os_string())))
        .collect()
}

fn env_arguments(environment: &[(OsString, OsString)]) -> Vec<OsString> {
    let mut args = vec![OsString::from("-i")];
    for (key, value) in environment {
        let mut item = key.clone();
        item.push("=");
        item.push(value);
        args.push(item);
    }
    args
}

fn child_arguments(
    executable: &Path,
    project: &Path,
    environment: &[(OsString, OsString)],
) -> Vec<OsString> {
    let mut args = env_arguments(environment);
    args.extend([
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(RUN_IN_PROJECT),
        OsString::from("switchya-terminal"),
        project.as_os_str().to_owned(),
        executable.as_os_str().to_owned(),
    ]);
    args
}

#[cfg(target_os = "linux")]
fn linux_command(
    executable: &Path,
    project: &Path,
    environment: &[(OsString, OsString)],
) -> Result<Command> {
    let display = environment
        .iter()
        .any(|(key, value)| (key == "DISPLAY" || key == "WAYLAND_DISPLAY") && !value.is_empty());
    if !display {
        return Err(AdapterError::Unavailable(
            "No graphical display is available for an interactive terminal.".into(),
        ));
    }
    let env = ["/usr/bin/env", "/bin/env"]
        .into_iter()
        .map(Path::new)
        .find(|path| native_executable(path))
        .ok_or_else(|| {
            AdapterError::Unavailable("The system environment launcher is unavailable.".into())
        })?;
    // These programs have documented argv execution modes. Do not accept a shell
    // command from $TERMINAL or use an unknown desktop launcher as a fallback.
    let (name, terminal) = ["gnome-terminal", "konsole", "xterm"].into_iter()
        .find_map(|name| find_terminal(name, project).map(|path| (name, path)))
        .ok_or_else(|| AdapterError::Unavailable(
            "No supported terminal was found. Install GNOME Terminal, Konsole, or xterm and try again.".into(),
        ))?;
    let mut command = Command::new(terminal);
    match name {
        "gnome-terminal" => {
            command
                .args(["--wait", "--working-directory"])
                .arg(project)
                .arg("--");
        }
        "konsole" => {
            command
                .args(["--separate", "--builtin-profile", "--workdir"])
                .arg(project)
                .arg("-e");
        }
        _ => {
            command.arg("-e");
        }
    }
    command
        .arg(env)
        .args(child_arguments(executable, project, environment));
    Ok(command)
}

#[cfg(target_os = "linux")]
fn find_terminal(name: &str, project: &Path) -> Option<PathBuf> {
    let mut directories = vec![
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path).filter(|path| path.is_absolute()));
    }
    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|path| {
            native_executable(path)
                && path
                    .canonicalize()
                    .is_ok_and(|actual| !actual.starts_with(project))
        })
}

#[cfg(target_os = "macos")]
const TERMINAL_SCRIPT: &str = r#"on run argv
    set commandText to "exec /usr/bin/env"
    repeat with i from 1 to count of argv
        set commandText to commandText & " " & quoted form of (item i of argv)
    end repeat
    with timeout of 4 seconds
        tell application id "com.apple.Terminal"
            activate
            do script commandText
        end tell
    end timeout
end run"#;

#[cfg(target_os = "macos")]
fn macos_command(
    executable: &Path,
    project: &Path,
    environment: &[(OsString, OsString)],
) -> Result<Command> {
    if !native_executable(Path::new("/usr/bin/osascript"))
        || !native_executable(Path::new("/usr/bin/env"))
        || ![
            "/System/Applications/Utilities/Terminal.app",
            "/Applications/Utilities/Terminal.app",
        ]
        .iter()
        .any(|path| Path::new(path).is_dir())
    {
        return Err(AdapterError::Unavailable(
            "macOS Terminal is unavailable.".into(),
        ));
    }
    let args = child_arguments(executable, project, environment);
    if args.iter().any(|value| {
        value
            .to_str()
            .is_none_or(|value| value.chars().any(char::is_control))
    }) {
        return Err(AdapterError::Invalid(
            "Terminal launch requires paths and environment values without control characters."
                .into(),
        ));
    }
    // User values are argv, never AppleScript source. `quoted form` is Apple's
    // documented shell-argument escaping, applied separately to every item.
    let mut command = Command::new("/usr/bin/osascript");
    // End osascript option parsing before env's leading -i or sh's later -c.
    command.args(["-e", TERMINAL_SCRIPT, "--"]).args(args);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> ProfileBinding {
        ProfileBinding {
            id: "fixture".into(),
            name: "Fixture".into(),
            agent_id: "claude".into(),
            home: PathBuf::from("/private/profile with 'quotes' $(literal)"),
            managed: true,
        }
    }

    #[test]
    fn terminal_environment_is_private_and_ignores_credential_and_loader_overrides() {
        let environment = environment(
            &binding(),
            [
                ("HOME", "/home/person"),
                ("PATH", ".:/usr/bin:relative:/bin"),
                ("ANTHROPIC_API_KEY", "not-forwarded"),
                ("OTHER_SERVICE_TOKEN", "not-forwarded"),
                ("CLAUDE_CONFIG_DIR", "/wrong"),
                ("NODE_OPTIONS", "not-forwarded"),
                ("LD_PRELOAD", "not-forwarded"),
                ("SHELL", "/untrusted"),
                ("DISPLAY", ":0"),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value.into())),
        );
        let values: std::collections::BTreeMap<_, _> = environment.into_iter().collect();
        assert_eq!(
            values.get(OsStr::new("PATH")),
            Some(&OsString::from("/usr/bin:/bin"))
        );
        assert_eq!(
            values.get(OsStr::new("CLAUDE_CONFIG_DIR")),
            Some(&binding().home.into_os_string())
        );
        assert_eq!(
            values.get(OsStr::new("DISPLAY")),
            Some(&OsString::from(":0"))
        );
        for key in [
            "ANTHROPIC_API_KEY",
            "OTHER_SERVICE_TOKEN",
            "NODE_OPTIONS",
            "LD_PRELOAD",
            "SHELL",
        ] {
            assert!(!values.contains_key(OsStr::new(key)));
        }
    }

    #[test]
    fn environment_values_remain_single_arguments() {
        let value = OsString::from("/private/profile 'quoted' $(literal) ; &");
        let args = env_arguments(&[(OsString::from("CLAUDE_CONFIG_DIR"), value)]);
        assert_eq!(
            args,
            [
                OsString::from("-i"),
                OsString::from("CLAUDE_CONFIG_DIR=/private/profile 'quoted' $(literal) ; &")
            ]
        );
    }

    #[test]
    fn project_and_executable_are_data_for_a_fixed_program() {
        let project = Path::new("/tmp/project 'quoted' $(literal) ; &");
        let executable = Path::new("/tmp/name=literal/claude");
        let args = child_arguments(executable, project, &[]);
        assert_eq!(args[0], "-i");
        assert_eq!(args[1], "/bin/sh");
        assert_eq!(args[2], "-c");
        assert_eq!(args[3], RUN_IN_PROJECT);
        assert_eq!(args[5], project.as_os_str());
        assert_eq!(args[6], executable.as_os_str());
        assert!(!args[3].to_string_lossy().contains("literal"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_graphical_display_fails_without_starting_a_terminal() {
        let error = linux_command(Path::new("/bin/true"), Path::new("/tmp"), &[]).unwrap_err();
        assert!(matches!(error, AdapterError::Unavailable(_)));
    }
}
