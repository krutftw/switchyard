use super::{CommandArgs, ToolResult, files};
use serde_json::json;
use std::{
    io,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT_BYTES: usize = 128 * 1024;

fn credential_environment(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    name.ends_with("_KEY")
        || [
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "PASSWD",
            "CREDENTIAL",
            "APIKEY",
            "API_KEY",
            "AUTHORIZATION",
        ]
        .iter()
        .any(|part| name.contains(part))
        || matches!(
            name.as_str(),
            "SSH_AUTH_SOCK" | "GIT_ASKPASS" | "SSH_ASKPASS" | "GIT_CONFIG_PARAMETERS"
        )
        || name.starts_with("GIT_CONFIG_KEY_")
        || name.starts_with("GIT_CONFIG_VALUE_")
}

pub(super) fn shell_label() -> &'static str {
    if cfg!(windows) {
        "Windows PowerShell (-NoProfile -NonInteractive)"
    } else {
        "/bin/sh"
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
    read_error: Option<String>,
}

async fn drain<R: AsyncRead + Unpin>(mut reader: R, capture: Arc<Mutex<Captured>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(count) => {
                let mut captured = capture.lock().expect("output capture lock poisoned");
                let retain = count.min(MAX_OUTPUT_BYTES.saturating_sub(captured.bytes.len()));
                captured.bytes.extend_from_slice(&chunk[..retain]);
                captured.truncated |= retain < count;
            }
            Err(error) => {
                capture
                    .lock()
                    .expect("output capture lock poisoned")
                    .read_error = Some(error.to_string());
                break;
            }
        }
    }
}

pub(super) async fn execute(
    root: &Path,
    args: &CommandArgs,
    cancel: CancellationToken,
) -> ToolResult {
    let cwd = match files::resolve(root, &args.cwd, true, false) {
        Ok(cwd) if cwd.is_dir() => cwd,
        Ok(_) => return ToolResult::failed("command cwd is no longer a directory"),
        Err(error) => return ToolResult::failed(error),
    };
    if cancel.is_cancelled() {
        return ToolResult::cancelled();
    }
    #[cfg(windows)]
    let mut command = {
        // Avoid PATH-based shell substitution, and do not load user profiles.
        let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let shell =
            std::path::PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = Command::new(shell);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &args.command,
        ]);
        command.creation_flags(
            windows_sys::Win32::System::Threading::CREATE_SUSPENDED
                | windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
        );
        command
    };
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &args.command]);
        command.process_group(0);
        command
    };
    command
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Build commands do not need the gateway's provider credentials, app host
    // token or other recognizable inherited auth secrets. This is defense in
    // depth only: an approved local shell still has normal filesystem rights.
    for (name, _) in std::env::vars_os() {
        if credential_environment(&name) {
            command.env_remove(name);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ToolResult::failed(format!("could not start shell: {error}")),
    };
    let mut tree = match ProcessTree::attach(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return ToolResult::failed(format!("could not establish process cleanup: {error}"));
        }
    };
    let stdout_capture = Arc::new(Mutex::new(Captured::default()));
    let stderr_capture = Arc::new(Mutex::new(Captured::default()));
    let mut stdout_task = tokio::spawn(drain(
        child.stdout.take().expect("stdout was piped"),
        stdout_capture.clone(),
    ));
    let mut stderr_task = tokio::spawn(drain(
        child.stderr.take().expect("stderr was piped"),
        stderr_capture.clone(),
    ));
    let timeout = tokio::time::sleep(Duration::from_millis(args.timeout_ms));
    tokio::pin!(timeout);
    let (state, wait_result) = tokio::select! {
        biased;
        _ = cancel.cancelled() => ("cancelled", None),
        _ = &mut timeout => ("timed_out", None),
        result = child.wait() => ("exited", Some(result)),
    };
    // Also end background children when the foreground shell returns. A command
    // tool is bounded work, never an implicit long-lived server launcher.
    let cleanup_error = tree.terminate().err().map(|error| error.to_string());
    let exit = if let Some(result) = wait_result {
        result
    } else {
        let _ = child.start_kill();
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("shell did not exit after termination")),
        }
    };
    // A deliberately detached Unix process can retain a pipe. The command runner
    // is not a security sandbox; never let inherited pipes hang the agent.
    let stdout_finished = matches!(
        tokio::time::timeout(Duration::from_secs(2), &mut stdout_task).await,
        Ok(Ok(()))
    );
    let stderr_finished = matches!(
        tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await,
        Ok(Ok(()))
    );
    if !stdout_finished {
        stdout_task.abort();
    }
    if !stderr_finished {
        stderr_task.abort();
    }
    let stdout = stdout_capture.lock().expect("output capture lock poisoned");
    let stderr = stderr_capture.lock().expect("output capture lock poisoned");
    let exit_code = exit.as_ref().ok().and_then(|status| status.code());
    let success = exit.as_ref().is_ok_and(|status| status.success());
    let status = match state {
        "cancelled" => "cancelled",
        "timed_out" => "timed_out",
        _ if success && cleanup_error.is_none() => "completed",
        _ => "failed",
    };
    let error = match state {
        "cancelled" => Some("command cancelled; process cleanup requested".to_owned()),
        "timed_out" => Some(format!(
            "command exceeded its {} ms timeout; process cleanup requested",
            args.timeout_ms
        )),
        _ => exit
            .err()
            .map(|error| error.to_string())
            .or(cleanup_error)
            .or_else(|| {
                (!success).then(|| format!("command exited unsuccessfully ({exit_code:?})"))
            }),
    };
    ToolResult {
        status: status.into(),
        error,
        exit_code,
        output: json!({"stdout":String::from_utf8_lossy(&stdout.bytes),"stderr":String::from_utf8_lossy(&stderr.bytes),
            "stdout_read_error":stdout.read_error,"stderr_read_error":stderr.read_error,"cwd":args.cwd,
            "shell":shell_label(),"sandboxed":false}),
        truncated: stdout.truncated
            || stderr.truncated
            || stdout.read_error.is_some()
            || stderr.read_error.is_some()
            || !stdout_finished
            || !stderr_finished,
        changes: vec![],
    }
}

#[cfg(unix)]
struct ProcessTree {
    pgid: i32,
    active: bool,
}

#[cfg(unix)]
impl ProcessTree {
    fn attach(child: &Child) -> io::Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no process id"))?;
        let pgid = i32::try_from(pid).map_err(|_| io::Error::other("invalid process group id"))?;
        if pgid <= 0 {
            return Err(io::Error::other("invalid process group id"));
        }
        Ok(Self { pgid, active: true })
    }

    fn terminate(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        // SAFETY: pgid is the positive PID of our freshly spawned process group.
        // Negative PID addresses that group, never the agent's own process group.
        if unsafe { libc::kill(-self.pgid, libc::SIGKILL) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: std::os::windows::io::OwnedHandle,
    active: bool,
}

#[cfg(windows)]
impl ProcessTree {
    fn attach(child: &Child) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // The shell was created suspended; it cannot launch children before it
        // joins the job. Every setup failure leaves the shell suspended and killed.
        // SAFETY: null pointers request default security and an unnamed job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the newly created handle is valid and exclusively owned here.
        let job = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: pointers and size describe a live, correctly typed limits value.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child process handle is unavailable"))?;
        // SAFETY: both handles are live and refer to our job and suspended child.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let tree = Self { job, active: true };
        resume_primary_thread(
            child
                .id()
                .ok_or_else(|| io::Error::other("child process id is unavailable"))?,
        )?;
        Ok(tree)
    }

    fn terminate(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if !self.active {
            return Ok(());
        }
        // SAFETY: job is a live owned handle, and termination affects only this job.
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        self.active = false;
        Ok(())
    }
}

#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    // SAFETY: the snapshot flags require no pointers or memory supplied by caller.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful snapshot returns an owned, closable handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: entry is initialized with the required size and remains live.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: request access only to the thread belonging to our child.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: OpenThread returned a new owned handle.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            // SAFETY: the shell is still suspended and this is its primary thread.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        // SAFETY: same valid snapshot and sized entry as Thread32First.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::other(
        "could not locate the suspended shell thread",
    ))
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(test)]
mod environment_tests {
    use super::credential_environment;
    use std::ffi::OsStr;

    #[test]
    fn build_environment_keeps_paths_but_not_inherited_credentials() {
        for key in [
            "OPENAI_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "SWITCHYARD_ADMIN_SECRET",
            "SWITCHYARD_CLIENT_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "SSH_AUTH_SOCK",
            "GIT_CONFIG_VALUE_0",
        ] {
            assert!(credential_environment(OsStr::new(key)), "{key}");
        }
        for key in [
            "PATH",
            "HOME",
            "SystemRoot",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "TEMP",
            "NODE_ENV",
        ] {
            assert!(!credential_environment(OsStr::new(key)), "{key}");
        }
    }
}
