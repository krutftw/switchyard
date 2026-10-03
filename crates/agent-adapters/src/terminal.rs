use crate::{AdapterError, ProfileBinding, Result};
use std::path::Path;

/// An explicit user action starts the official CLI in its own interactive
/// terminal. This external session is not tracked or stopped by Switchya.
pub fn launch_profile_terminal(binding: &ProfileBinding, project: &Path) -> Result<()> {
    crate::profile::validate(binding, "claude")?;
    if !binding.managed {
        return Err(AdapterError::Invalid(
            "Only managed Claude profiles can open an external terminal.".into(),
        ));
    }
    if !project.is_absolute() || !project.is_dir() {
        return Err(AdapterError::Invalid(
            "Open a project directory before starting Claude Code.".into(),
        ));
    }
    let executable = crate::discovery::find_cli("claude").ok_or_else(|| {
        AdapterError::Unavailable("Claude Code native executable was not found.".into())
    })?;
    launch(&executable, binding, project)
}

#[cfg(unix)]
fn launch(executable: &Path, binding: &ProfileBinding, project: &Path) -> Result<()> {
    crate::terminal_unix::launch(executable, binding, project)
}

#[cfg(not(any(windows, unix)))]
fn launch(_executable: &Path, _binding: &ProfileBinding, _project: &Path) -> Result<()> {
    Err(AdapterError::Unavailable(
        "Opening a profile in an external terminal is not implemented for this platform.".into(),
    ))
}

#[cfg(windows)]
fn launch(executable: &Path, binding: &ProfileBinding, project: &Path) -> Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION,
            STARTUPINFOW,
        },
    };
    fn wide(value: &OsStr) -> Result<Vec<u16>> {
        let mut result: Vec<_> = value.encode_wide().collect();
        if result.contains(&0) {
            return Err(AdapterError::Invalid(
                "Terminal paths cannot contain NUL characters.".into(),
            ));
        }
        result.push(0);
        Ok(result)
    }
    // Use the same scoped environment sanitization as account/run children.
    let mut prepared = tokio::process::Command::new(executable);
    crate::profile::configure(&mut prepared, Some(binding));
    let mut environment: std::collections::BTreeMap<_, _> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().to_ascii_uppercase(), (k, v)))
        .collect();
    for (key, value) in prepared.as_std().get_envs() {
        let index = key.to_string_lossy().to_ascii_uppercase();
        if let Some(value) = value {
            environment.insert(index, (key.to_owned(), value.to_owned()));
        } else {
            environment.remove(&index);
        }
    }
    let mut block = Vec::new();
    for (_, (key, value)) in environment {
        let mut item = key;
        item.push("=");
        item.push(value);
        block.extend(wide(&item)?);
    }
    block.push(0);
    let executable = wide(executable.as_os_str())?;
    let directory = wide(project.as_os_str())?;
    let mut command_line = vec![b'"' as u16];
    command_line.extend_from_slice(&executable[..executable.len() - 1]);
    command_line.extend([b'"' as u16, 0]);
    // No STARTF_USESTDHANDLES: Windows supplies the NEW console's interactive
    // handles instead of inheriting the app host's pipes. No shell is involved.
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: all buffers are NUL terminated and remain alive for the call;
    // environment is a double-NUL-terminated Unicode block. Handles are not
    // inherited, and lpApplicationName is the discovered fixed native binary.
    let ok = unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            block.as_ptr().cast(),
            directory.as_ptr(),
            &startup,
            &mut process,
        )
    };
    if ok == 0 {
        return Err(AdapterError::Unavailable(
            "Windows could not open Claude Code in a new terminal.".into(),
        ));
    }
    // SAFETY: successful CreateProcessW returns these two owned handles. The
    // user-owned external terminal intentionally continues after handles close.
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}
