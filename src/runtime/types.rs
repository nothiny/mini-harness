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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct EventSeq(pub u64);
impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 一次工具调用在 reducer 状态机中的位置。
///
/// `ToolRequested` 事件创建 `Pending`；`ToolCompleted` 事件将其推进到
/// `Completed` 或 `Failed`。被中断且结果不可知的调用停留在
/// `OutcomeUnknown` —— 它们阻止 turn 正常完成。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionState {
    Pending,
    Completed { result: ToolResult },
    Failed { error: ExecutionError },
    OutcomeUnknown,
}
