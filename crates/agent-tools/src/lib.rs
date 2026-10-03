//! Local tools for the agent engine.
//!
//! `prepare` seals the exact arguments and patch preview. Keep that value in memory
//! while obtaining approval, then execute the same value. This crate deliberately
//! does not authenticate approvals: the engine must approve every patch and command.
//! File tools refuse links, reparse points, repository metadata and likely secrets.
//! Commands have the user's machine permissions; a project cwd is **not a sandbox**.

mod command;
mod files;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_PATCH_BYTES: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Invalid(String),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid tool arguments: {0}")]
    Json(#[from] serde_json::Error),
}

/// An in-memory, sealed execution plan. Never reconstruct this from approved JSON.
#[derive(Clone, Debug)]
pub struct PreparedTool {
    pub name: String,
    pub args: Value,
    pub arguments_hash: String,
    pub requires_approval: bool,
    pub preview: Value,
    root: PathBuf,
    plan: Plan,
    sealed_preview: Value,
    sealed_hash: String,
}

#[derive(Clone, Debug)]
enum Plan {
    Read(ReadArgs),
    Search(SearchArgs),
    Patch(Vec<files::PreparedEdit>),
    Command(CommandArgs),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_read_bytes")]
    max_bytes: usize,
}

fn default_read_bytes() -> usize {
    64 * 1024
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_cwd")]
    path: String,
    #[serde(default = "default_search_results")]
    max_results: usize,
}

fn default_search_results() -> usize {
    30
}
fn default_cwd() -> String {
    ".".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    edits: Vec<EditArgs>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    /// Null is permitted only when creating a file that does not already exist.
    #[serde(deserialize_with = "required_optional_hash")]
    before_sha256: Option<String>,
    content: String,
}

fn required_optional_hash<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: String,
    #[serde(default = "default_cwd")]
    cwd: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub before_sha256: Option<String>,
    pub after_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    /// completed, failed, cancelled, or timed_out
    pub status: String,
    pub error: Option<String>,
    pub output: Value,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub changes: Vec<ChangedFile>,
}

impl ToolResult {
    fn completed(output: Value) -> Self {
        Self {
            status: "completed".into(),
            error: None,
            output,
            exit_code: None,
            truncated: false,
            changes: vec![],
        }
    }

    fn failed(error: impl ToString) -> Self {
        Self {
            status: "failed".into(),
            error: Some(error.to_string()),
            output: Value::Null,
            exit_code: None,
            truncated: false,
            changes: vec![],
        }
    }

    fn cancelled() -> Self {
        Self {
            status: "cancelled".into(),
            ..Self::failed("tool execution cancelled")
        }
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn arguments_hash(root: &Path, name: &str, args: &Value) -> String {
    sha256(
        serde_json::to_string(&json!({"project_root":root,"name":name,"args":args}))
            .expect("serializing a JSON value is infallible")
            .as_bytes(),
    )
}

/// Validate arguments and freeze the exact patch/command to be approved.
pub fn prepare(project_root: &Path, name: &str, args: &Value) -> Result<PreparedTool, ToolError> {
    let root = project_root.canonicalize()?;
    if !root.is_dir() {
        return Err(ToolError::Invalid(
            "project root must be a directory".into(),
        ));
    }
    let (plan, args, preview, requires_approval) = match name {
        "read_file" => {
            let a: ReadArgs = serde_json::from_value(args.clone())?;
            if !(1..=MAX_FILE_BYTES).contains(&a.max_bytes) {
                return Err(ToolError::Invalid(format!(
                    "max_bytes must be 1..={MAX_FILE_BYTES}"
                )));
            }
            files::resolve(&root, &a.path, false, false)?;
            let value = serde_json::to_value(&a)?;
            (Plan::Read(a), value.clone(), value, false)
        }
        "search_files" => {
            let a: SearchArgs = serde_json::from_value(args.clone())?;
            if a.query.is_empty() || a.query.len() > 512 || !(1..=100).contains(&a.max_results) {
                return Err(ToolError::Invalid(
                    "query must contain 1..=512 bytes and max_results must be 1..=100".into(),
                ));
            }
            let path = files::resolve(&root, &a.path, true, false)?;
            if !path.is_dir() {
                return Err(ToolError::Invalid("search path must be a directory".into()));
            }
            let value = serde_json::to_value(&a)?;
            (Plan::Search(a), value.clone(), value, false)
        }
        "apply_patch" => {
            let a: PatchArgs = serde_json::from_value(args.clone())?;
            let edits = files::prepare_edits(&root, &a.edits)?;
            let preview = json!({"edits":edits.iter().map(|e| e.preview()).collect::<Vec<_>>(),
                "atomic_batch":false, "note":"Each file is replaced atomically after its current hash is checked; a filesystem failure may leave a partially applied batch."});
            (Plan::Patch(edits), serde_json::to_value(a)?, preview, true)
        }
        "run_command" => {
            let a: CommandArgs = serde_json::from_value(args.clone())?;
            if a.command.trim().is_empty()
                || a.command.len() > 16 * 1024
                || a.command.contains('\0')
            {
                return Err(ToolError::Invalid(
                    "command must contain 1..=16384 bytes and no NUL characters".into(),
                ));
            }
            if !(1..=300_000).contains(&a.timeout_ms) {
                return Err(ToolError::Invalid("timeout_ms must be 1..=300000".into()));
            }
            let cwd = files::resolve(&root, &a.cwd, true, false)?;
            if !cwd.is_dir() {
                return Err(ToolError::Invalid("command cwd must be a directory".into()));
            }
            let preview = json!({"command":a.command,"cwd":a.cwd,"absolute_cwd":cwd,
                "shell":command::shell_label(),"timeout_ms":a.timeout_ms,
                "sandboxed":false,"warning":"This command runs with your account's machine and network access. The working directory is not a sandbox."});
            (
                Plan::Command(a.clone()),
                serde_json::to_value(a)?,
                preview,
                true,
            )
        }
        _ => return Err(ToolError::Invalid(format!("unknown tool: {name}"))),
    };
    let arguments_hash = arguments_hash(&root, name, &args);
    Ok(PreparedTool {
        name: name.into(),
        args,
        sealed_hash: arguments_hash.clone(),
        arguments_hash,
        requires_approval,
        sealed_preview: preview.clone(),
        preview,
        root,
        plan,
    })
}

/// Execute a previously prepared plan. The caller is responsible for approval.
pub async fn execute(prepared: &PreparedTool, cancel: CancellationToken) -> ToolResult {
    if cancel.is_cancelled() {
        return ToolResult::cancelled();
    }
    let (name, requires_approval) = match &prepared.plan {
        Plan::Read(_) => ("read_file", false),
        Plan::Search(_) => ("search_files", false),
        Plan::Patch(_) => ("apply_patch", true),
        Plan::Command(_) => ("run_command", true),
    };
    if prepared.name != name
        || prepared.requires_approval != requires_approval
        || prepared.preview != prepared.sealed_preview
        || prepared.arguments_hash != prepared.sealed_hash
        || arguments_hash(&prepared.root, &prepared.name, &prepared.args) != prepared.arguments_hash
    {
        return ToolResult::failed("prepared tool metadata changed after validation");
    }
    if let Plan::Command(args) = &prepared.plan {
        return command::execute(&prepared.root, args, cancel).await;
    }
    let prepared = prepared.clone();
    match tokio::task::spawn_blocking(move || match prepared.plan {
        Plan::Read(args) => files::read(&prepared.root, &args, &cancel),
        Plan::Search(args) => files::search(&prepared.root, &args, &cancel),
        Plan::Patch(edits) => files::apply(&prepared.root, &edits, &cancel),
        Plan::Command(_) => unreachable!(),
    })
    .await
    {
        Ok(result) => result,
        Err(error) => ToolResult::failed(format!("tool worker failed: {error}")),
    }
}

/// OpenAI Responses function definitions. Server-side typed validation is authoritative.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","name":"read_file","description":"Read a bounded UTF-8 project file. Returns sha256 only when the entire file fits. Repository metadata, secrets and links are blocked.","parameters":{"type":"object","properties":{"path":{"type":"string"},"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_FILE_BYTES}},"required":["path"],"additionalProperties":false}}),
        json!({"type":"function","name":"search_files","description":"Search project filenames and UTF-8 file content for a literal, case-sensitive query. Bounded traversal excludes secrets, links, dependencies and build directories.","parameters":{"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":100}},"required":["query"],"additionalProperties":false}}),
        json!({"type":"function","name":"apply_patch","description":"Propose exact replacement content with an approval preview. Use before_sha256 from a complete read; null creates a new file only. Existing parent directories are required. Execution requires human approval and rejects changed files.","parameters":{"type":"object","properties":{"edits":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"object","properties":{"path":{"type":"string"},"before_sha256":{"type":["string","null"]},"content":{"type":"string"}},"required":["path","before_sha256","content"],"additionalProperties":false}}},"required":["edits"],"additionalProperties":false}}),
        json!({"type":"function","name":"run_command","description":"Request human approval for a shell command in a project directory. Commands run with full user permissions, not in a sandbox. Output is bounded; timeout and cancellation terminate the process group/job.","parameters":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":300000}},"required":["command"],"additionalProperties":false}}),
    ]
}
