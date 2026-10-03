use crate::{
    AdapterStatus,
    process_tree::{self, ProcessTree},
    types::PERMISSION_BOUNDARY,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

/// Resolve only native executable names. Windows .cmd/.bat shims are deliberately
/// excluded: a shell must never interpret a discovered path or the user prompt.
pub(crate) fn find_cli(id: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{id}.exe")
    } else {
        id.to_owned()
    };
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = PathBuf::from(home);
        match id {
            "codex" => {
                if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                    candidates.push(
                        PathBuf::from(local)
                            .join("Programs/OpenAI/Codex/bin")
                            .join(&name),
                    );
                }
                candidates.push(home.join(".local/bin").join(&name));
                candidates.push(home.join(".cargo/bin").join(&name));
            }
            "claude" => candidates.push(home.join(".local/bin").join(&name)),
            "omp" => candidates.push(home.join(".bun/bin").join(&name)),
            _ => return None,
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        // Empty/relative PATH elements could make the project control execution.
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(&name)),
        );
    }
    #[cfg(target_os = "macos")]
    if id == "codex" {
        candidates.push(PathBuf::from(
            "/Applications/Codex.app/Contents/Resources/codex",
        ));
    }
    candidates.into_iter().find_map(|p| {
        if !p.is_file() {
            return None;
        }
        let canonical = p.canonicalize().ok()?;
        #[cfg(windows)]
        if canonical
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|s| !s.eq_ignore_ascii_case("exe"))
        {
            return None;
        }
        Some(canonical)
    })
}

async fn version(path: &Path) -> Option<String> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    process_tree::configure(&mut command);
    let mut child = command.spawn().ok()?;
    let mut tree = match ProcessTree::attach(&child) {
        Ok(tree) => tree,
        Err(_) => {
            let _ = child.kill().await;
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        stdout.take(4097).read_to_end(&mut bytes).await.ok()?;
        if bytes.len() > 4096 {
            return None;
        }
        let status = child.wait().await.ok()?;
        if !status.success() {
            return None;
        }
        let output = String::from_utf8(bytes).ok()?;
        let line = output.trim();
        if line.is_empty()
            || line
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\r')
        {
            return None;
        }
        Some(line.chars().take(200).collect())
    })
    .await
    .ok()
    .flatten();
    let _ = tree.terminate();
    let _ = child.kill().await;
    result
}

pub(crate) async fn discover() -> Vec<AdapterStatus> {
    let mut statuses = Vec::with_capacity(3);
    for (id, name) in [
        ("codex", "Codex"),
        ("claude", "Claude Code"),
        ("omp", "Oh My Pi"),
    ] {
        let executable = find_cli(id);
        let installed = executable.is_some();
        let detected_version = match &executable {
            Some(path) => version(path).await,
            None => None,
        };
        let supported = id == "codex";
        let status = if !installed {
            "not_installed"
        } else if !supported {
            "integration_not_implemented"
        } else if detected_version.is_none() {
            "version_check_failed"
        } else {
            "available"
        };
        statuses.push(AdapterStatus {
            id: id.into(),
            name: name.into(),
            installed,
            supported,
            version: detected_version,
            executable: executable.map(|p| p.to_string_lossy().into_owned()),
            status: status.into(),
            permission_boundary: if supported {
                PERMISSION_BOUNDARY.into()
            } else {
                "Discovery only. Starting runs is not supported yet.".into()
            },
        });
    }
    statuses
}
