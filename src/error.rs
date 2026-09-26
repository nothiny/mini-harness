use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("configuration error: {0}")]
    Config(ConfigError),
    #[error("durable error: {0}")]
    Durable(#[from] DurableError),
    #[error("provider error: {0}")]
    Provider(ProviderError),
    #[error("tool error: {0}")]
    Tool(ToolError),
    #[error("execution error: {0}")]
    Execution(ExecutionError),
    #[error("policy error: {0}")]
    Policy(PolicyError),
    #[error("approval pending: {0}")]
    ApprovalPending(PolicyError),
    #[error("cancelled")]
    Cancelled,
    #[error("turn timed out")]
    Timeout,
    #[error("invariant violation: {0}")]
    InvariantViolation(String),
    #[error("turn already active")]
    TurnAlreadyActive,
    #[error("input queue is full; the session cannot accept more queued inputs")]
    QueueLimitExceeded,
}

macro_rules! message_error {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum $name {
            Message { message: String },
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Message { message } => message.fmt(f),
                }
            }
        }
        impl From<String> for $name {
            fn from(message: String) -> Self {
                Self::Message { message }
            }
        }
        impl From<&str> for $name {
            fn from(message: &str) -> Self {
                Self::from(message.to_owned())
            }
        }
    };
}
message_error!(ConfigError);
message_error!(ProviderError);
message_error!(ToolError);
message_error!(PolicyError);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Error)]
pub enum ReadFileError {
    #[error("workspace root {path:?} is unavailable: {message}")]
    WorkspaceRoot { path: PathBuf, message: String },
    #[error("path {path:?} escapes the workspace")]
    OutsideWorkspace { path: PathBuf },
    #[error("file {path:?} does not exist")]
    NotFound { path: PathBuf },
    #[error("path {path:?} is a directory")]
    IsDirectory { path: PathBuf },
    #[error("permission denied for {path:?}")]
    PermissionDenied { path: PathBuf },
    #[error("path {path:?} is not a regular file")]
    NotRegularFile { path: PathBuf },
    #[error("file {path:?} is not valid UTF-8 text")]
    BinaryFile { path: PathBuf },
    #[error("failed to read {path:?}: {message}")]
    Io { path: PathBuf, message: String },
}

impl ReadFileError {
    pub(crate) fn from_io(path: PathBuf, error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound { path },
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied { path },
            std::io::ErrorKind::IsADirectory => Self::IsDirectory { path },
            _ => Self::Io {
                path,
                message: error.to_string(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Error)]
pub enum ProcessError {
    #[error("process execution is not supported by this executor")]
    Unsupported,
    #[error("workspace root is unavailable: {message}")]
    WorkspaceRoot { message: String },
    #[error("failed to spawn process: {message}")]
    Spawn { message: String },
    #[error("process IO failed: {message}")]
    Io { message: String },
    #[error("process cancelled")]
    Cancelled,
    #[error("process concurrency limit reached; no slot freed within the wait budget")]
    ConcurrencyLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Error)]
pub enum EditFileError {
    #[error("file editing is not supported by this executor")]
    Unsupported,
    #[error("file edit cancelled")]
    Cancelled,
    #[error("workspace root is unavailable: {message}")]
    WorkspaceRoot { message: String },
    #[error("path {path:?} escapes the workspace")]
    OutsideWorkspace { path: PathBuf },
    #[error("file {path:?} does not exist")]
    NotFound { path: PathBuf },
    #[error("path {path:?} is not a regular file")]
    NotRegularFile { path: PathBuf },
    #[error("file {path:?} is not valid UTF-8 text")]
    BinaryFile { path: PathBuf },
    #[error("{field} exceeds the {max_bytes} byte limit")]
    TooLarge { field: String, max_bytes: usize },
    #[error("patch text was not found in {path:?}")]
    PatchNotFound { path: PathBuf },
    #[error("patch text appears more than once in {path:?}")]
    AmbiguousPatch { path: PathBuf },
    #[error("file changed before edit: expected {expected}, found {actual}")]
    Conflict { expected: String, actual: String },
    #[error("permission denied for {path:?}")]
    PermissionDenied { path: PathBuf },
    #[error("failed to edit {path:?}: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionError {
    Spawn { message: String },
    Timeout { message: String },
    ExitStatus { code: Option<i32>, message: String },
    Cancelled { message: String },
    Tool { error: ToolError },
    Message { message: String },
}
impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { message }
            | Self::Timeout { message }
            | Self::Cancelled { message }
            | Self::Message { message } => message.fmt(f),
            Self::ExitStatus { code, message } => write!(f, "{message} (exit code {code:?})"),
            Self::Tool { error } => error.fmt(f),
        }
    }
}
impl From<String> for ExecutionError {
    fn from(message: String) -> Self {
        Self::Message { message }
    }
}
impl From<&str> for ExecutionError {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

#[derive(Debug, Error)]
pub enum DurableError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("corrupt event log at line {0}")]
    Corrupt(usize),
    #[error("durable log limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("unsupported event schema version {0}")]
    UnsupportedSchema(u32),
    #[error("checkpoint error: {0}")]
    Checkpoint(String),
}
