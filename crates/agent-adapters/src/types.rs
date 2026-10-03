use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize)]
pub struct AdapterStatus {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub supported: bool,
    pub version: Option<String>,
    pub executable: Option<String>,
    pub status: String,
    pub permission_boundary: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub adapter_id: String,
    pub project_path: PathBuf,
    pub prompt: String,
    /// Caller UUID: retries return the same run only for the same complete request.
    pub command_id: String,
    /// Resolved by the trusted account manager, never accepted from HTTP JSON.
    #[serde(skip)]
    pub profile: Option<ProfileBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileBinding {
    pub id: String,
    pub name: String,
    pub agent_id: String,
    pub home: PathBuf,
    pub managed: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    AwaitingApproval,
    Interrupting,
    Interrupted,
    Completed,
    Failed,
    RecoveryRequired,
}

impl RunState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::AwaitingApproval | Self::Interrupting
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterRun {
    pub id: String,
    pub adapter_id: String,
    pub command_id: String,
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    pub project_path: String,
    pub state: RunState,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub last_seq: u64,
    pub first_retained_seq: u64,
    pub pending_approvals: Vec<AdapterApproval>,
    /// These runs are intentionally not restarted or replayed across host exits.
    pub ephemeral: bool,
    pub permission_boundary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterApproval {
    pub id: String,
    pub expected_hash: String,
    pub method: String,
    /// Exact CLI request plus the associated item, when present. Render as text.
    pub preview: Value,
    pub can_allow_once: bool,
    pub permission_boundary: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterEvent {
    pub schema_version: u32,
    pub run_id: String,
    pub seq: u64,
    pub at_ms: u64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Limit(String),
}

impl AdapterError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Unavailable(_) => "adapter_unavailable",
            Self::Protocol(_) => "adapter_protocol_error",
            Self::Limit(_) => "limit_reached",
        }
    }
}

pub type Result<T> = std::result::Result<T, AdapterError>;

pub const PERMISSION_BOUNDARY: &str = "Codex uses its read-only shell sandbox with network disabled, on-request approvals and human review. Read-only operations may run without prompting; writes or elevation require CLI approval. Codex enforces that policy; Switchya does not add an OS security sandbox. Allow once may permit the displayed command or file change with your OS permissions. The selected profile stays bound to this run; configured integrations may have separate permissions. Runs exist only until this host exits.";

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
