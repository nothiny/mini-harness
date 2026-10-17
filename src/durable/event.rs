use crate::{
    error::ToolError,
    runtime::{
        ids::*,
        types::{EventSeq, ModelText, ToolName, ToolResult, UserInput},
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// All durable facts the harness records.
///
/// 每个工具调用恰好产生两类事件：
///
/// 1. [`EventPayload::ToolRequested`] — 请求（含是否需要审批）
/// 2. [`EventPayload::ToolCompleted`] — 结果（成功或失败）
///
/// 审批决策（仅 `ask` 工具）额外产生一个
/// [`EventPayload::ToolApprovalResponded`] 事件，因为用户的决定本身
/// 也是需要持久化的事实。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    SessionCreated,
    UserInputRecorded {
        input: UserInput,
    },
    TurnStarted,
    ModelResponseRecorded {
        text: ModelText,
    },
    ProviderContinuationUpdated {
        continuation: Option<crate::runtime::ProviderContinuation>,
    },
    ProviderAttemptRecorded {
        attempt: u32,
        client_request_id: String,
        request_id: Option<String>,
        response_id: Option<String>,
        outcome: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        total_tokens: Option<u64>,
    },
    /// 记录一次工具调用请求。`requires_approval` 为 true 时，reducer 会
    /// 同时登记一个 Pending 审批 —— 客户端由此得知需要调用
    /// `approval.respond`。
    ToolRequested {
        call_id: ToolCallId,
        name: ToolName,
        input: serde_json::Value,
        requires_approval: bool,
    },
    /// 用户对 `ask` 工具的审批决定。`approved: false` 统一表示拒绝。
    ToolApprovalResponded {
        call_id: ToolCallId,
        approved: bool,
    },
    /// 工具执行结果（成功或失败合并为一个事件）。
    ToolCompleted {
        call_id: ToolCallId,
        outcome: ToolOutcome,
        duration_ms: u64,
    },
    /// 执行被中断且结果不可知（例如进程崩溃、取消时清理超时）。
    /// 与确定的失败不同：副作用可能已经发生。
    ToolOutcomeUnknown {
        call_id: ToolCallId,
        reason: String,
    },
    TurnCompleted {
        text: ModelText,
    },
    TurnFailed {
        error: String,
    },
    /// Records that cancellation was requested for an active turn.
    ///
    /// This is separate from [`TurnCancelled`](Self::TurnCancelled), which
    /// records the terminal transition after any in-flight work has stopped.
    TurnCancelRequested,
    TurnCancelled,
    TurnTimedOut,
}

/// 一次工具调用的确定结果。
///
/// `Rejected` 与 `Failed` 的区别是“副作用是否可能已发生”：
///
/// * [`ToolOutcome::Rejected`] — 执行前就被拒绝（工具不存在、参数
///   校验失败）。没有副作用，turn 可以继续，模型能在下一步重试。
/// * [`ToolOutcome::Failed`] — 执行后失败。副作用可能已发生
///   （例如命令已写了一半文件），turn 必须终止。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutcome {
    Succeeded { result: ToolResult },
    Rejected { error: ToolError },
    Failed { error: ToolError },
}

impl ToolOutcome {
    /// 对模型可见的文本形式（成功输出或错误信息）。
    pub fn as_tool_result(&self) -> ToolResult {
        match self {
            Self::Succeeded { result } => result.clone(),
            Self::Rejected { error } | Self::Failed { error } => ToolResult(error.to_string()),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }
}

impl EventPayload {
    /// Stable, human-readable event name shared by tracing, the JSONL
    /// protocol projection, and diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::UserInputRecorded { .. } => "user.input.recorded",
            Self::TurnStarted => "turn.started",
            Self::ModelResponseRecorded { .. } => "model.response.recorded",
            Self::ProviderContinuationUpdated { .. } => "provider.continuation.updated",
            Self::ProviderAttemptRecorded { .. } => "provider.attempt.recorded",
            Self::ToolRequested { .. } => "tool.requested",
            Self::ToolApprovalResponded { .. } => "tool.approval.responded",
            Self::ToolCompleted { .. } => "tool.completed",
            Self::ToolOutcomeUnknown { .. } => "tool.outcome_unknown",
            Self::TurnCompleted { .. } => "turn.completed",
            Self::TurnFailed { .. } => "turn.failed",
            Self::TurnCancelRequested => "turn.cancel_requested",
            Self::TurnCancelled => "turn.cancelled",
            Self::TurnTimedOut => "turn.timed_out",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: EventId,
    pub seq: EventSeq,
    pub timestamp: DateTime<Utc>,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub payload: EventPayload,
}
impl Event {
    pub const CURRENT_SCHEMA_VERSION: u32 = 3;
    pub fn new(session_id: SessionId, turn_id: Option<TurnId>, payload: EventPayload) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            event_id: EventId::new(),
            seq: EventSeq(0),
            timestamp: Utc::now(),
            session_id,
            turn_id,
            payload,
        }
    }
    pub fn from_json(input: &str) -> Result<Self, DurableError> {
        let event: Self = serde_json::from_str(input)?;
        // Schema 3 是简化版事件集（合并了工具事件）。旧日志（schema 1/2）
        // 使用不同的事件形状，无法直接重放 —— 简化版从新日志开始。
        if !matches!(event.schema_version, Self::CURRENT_SCHEMA_VERSION) {
            return Err(DurableError::UnsupportedSchema(event.schema_version));
        }
        Ok(event)
    }

    /// Upgrade a decoded legacy event to the current durable schema.
    pub fn migrate_to_current(mut self) -> Result<Self, DurableError> {
        if !matches!(self.schema_version, Self::CURRENT_SCHEMA_VERSION) {
            return Err(DurableError::UnsupportedSchema(self.schema_version));
        }
        self.schema_version = Self::CURRENT_SCHEMA_VERSION;
        Ok(self)
    }
}

use crate::error::DurableError;
