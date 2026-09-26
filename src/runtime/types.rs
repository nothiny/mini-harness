use crate::error::ExecutionError;
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf, time::Duration};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelName(pub String);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolName(pub String);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRoot(pub PathBuf);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UserInput(pub String);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelText(pub String);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResult(pub String);
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequestId(pub String);
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ByteLimit(pub usize);
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Timeout(pub u64);
impl Timeout {
    pub fn duration(self) -> Duration {
        Duration::from_millis(self.0)
    }
}

/// Describes the safe default action for a fact that was interrupted during recovery.
///
/// The policy is intentionally separate from the execution result: an unknown result
/// means that the external operation may have happened, while this value tells the
/// caller what must be done before continuing the turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RecoveryPolicy {
    RetryRead,
    InspectEditHash,
    RequireUserDecision,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct EventSeq(pub u64);
impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionState {
    Pending,
    Running {
        execution_id: super::ids::ExecutionId,
    },
    Completed {
        result: ToolResult,
    },
    Failed {
        error: ExecutionError,
    },
    Cancelled,
    OutcomeUnknown,
}
