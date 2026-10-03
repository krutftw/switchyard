use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// All session data is private local state under this directory.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub data_dir: PathBuf,
    pub max_steps: u32,
    pub max_output_tokens: u64,
    pub max_context_bytes: usize,
}

impl EngineOptions {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            max_steps: 16,
            max_output_tokens: 4096,
            max_context_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    AwaitingApproval,
    Interrupted,
    RecoveryRequired,
    Failed,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub model: String,
    pub title: String,
    pub state: SessionState,
    pub active_run_id: Option<String>,
    pub revision: u64,
    pub last_seq: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    pub command_id: String,
    pub state: SessionState,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub session_id: String,
    pub run_id: Option<String>,
    pub seq: u64,
    pub at_ms: i64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub call_id: String,
    pub name: String,
    pub arguments_hash: String,
    /// Exact normalized arguments bound to this single-use approval.
    pub arguments: Value,
    pub requires_approval: bool,
    pub state: String,
    pub preview: Value,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AllowOnce,
    Deny,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionView {
    pub session: Session,
    pub project: Project,
    pub pending_operation: Option<Operation>,
    /// Initial window of public events. Fetch later events with a sequence cursor.
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineStatus {
    pub active_runs: usize,
    pub permission_mode: String,
    pub shell_is_sandboxed: bool,
    pub max_steps: u32,
    pub max_output_tokens: u64,
    pub max_context_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Permission(String),
    #[error("{0}")]
    Gateway(String),
    #[error("local session storage failed: {0}")]
    Storage(String),
    #[error("the run was interrupted")]
    Interrupted,
    #[error("{0}")]
    Limit(String),
}

impl AgentError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Permission(_) => "permission_denied",
            Self::Gateway(_) => "gateway_failed",
            Self::Storage(_) => "storage_failed",
            Self::Interrupted => "interrupted",
            Self::Limit(_) => "limit_reached",
        }
    }
}

impl From<rusqlite::Error> for AgentError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AgentError>;

pub(crate) fn now_ms() -> i64 {
    switchyard_core::util::now_unix_ms()
}

pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}
