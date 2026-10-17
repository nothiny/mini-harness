use super::{
    ids::{SessionId, ToolCallId, TurnId},
    types::{EventSeq, ModelText, ToolName, ToolResult, UserInput},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use super::types::ExecutionState;

/// Provider-owned cursor used to continue a response after tool execution.
///
/// The response id and native call ids are persisted separately from the
/// normalized runtime ids so a resumed session can submit the exact
/// `function_call_output` items expected by the provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderContinuation {
    pub provider: String,
    pub response_id: Option<String>,
    pub model: String,
    pub endpoint: String,
    pub native_call_ids: HashMap<ToolCallId, String>,
    pub history_cursor: usize,
    /// Chain-of-thought from the latest thinking-model response (DeepSeek
    /// reasoner/flash). Must be passed back in the next request's assistant
    /// message alongside tool_calls; `#[serde(default)]` keeps old
    /// checkpoints/logs readable.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Reasoning for each assistant response in order.  DeepSeek requires
    /// `reasoning_content` on every assistant message it receives in thinking
    /// mode, including earlier assistant messages that are replayed around a
    /// tool call.  The optional entries preserve alignment when a response
    /// did not contain reasoning content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_history: Vec<Option<String>>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Idle,
    Running,
    Failed,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnState {
    pub turn_id: TurnId,
    pub status: TurnStatus,
    pub last_model_response: Option<ModelText>,
    pub tool_calls: Vec<ToolCallId>,
    /// 每个工具调用的执行状态机（Pending → Completed/Failed/OutcomeUnknown）。
    pub executions: HashMap<ToolCallId, ExecutionState>,
    pub tool_names: HashMap<ToolCallId, ToolName>,
    pub tool_inputs: HashMap<ToolCallId, serde_json::Value>,
    /// Calls rejected before execution (validation failures, denials) can be
    /// corrected by a later model response. Keep them distinct from failures
    /// after an effect may occur.
    #[serde(default)]
    pub rejected_tool_calls: HashSet<ToolCallId>,
    pub approvals: HashMap<ToolCallId, ApprovalStatus>,
    pub final_text: Option<ModelText>,
}

impl TurnState {
    /// Creates an empty running turn.
    pub fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            status: TurnStatus::Running,
            last_model_response: None,
            tool_calls: Vec::new(),
            executions: HashMap::new(),
            tool_names: HashMap::new(),
            tool_inputs: HashMap::new(),
            rejected_tool_calls: HashSet::new(),
            approvals: HashMap::new(),
            final_text: None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HistoryItem {
    User(UserInput),
    Assistant(ModelText),
    Tool {
        call_id: ToolCallId,
        name: ToolName,
        /// Original arguments are required when stateless providers replay
        /// assistant tool calls.
        #[serde(default)]
        input: serde_json::Value,
        result: ToolResult,
    },
}

/// Reducer 重建的全部会话状态。
///
/// 状态 = fold(events)。同一事件序列重放任意次都得到相同的状态。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: SessionId,
    pub status: SessionStatus,
    pub active_turn: Option<TurnState>,
    pub history: Vec<HistoryItem>,
    /// 已记录但尚未被 `turn.started` 消费的输入。简化版一次只允许一个：
    /// turn 运行期间到达的新输入会被直接拒绝。
    pub pending_input: Option<UserInput>,
    #[serde(default)]
    pub provider_continuation: Option<ProviderContinuation>,
    pub last_seq: EventSeq,
    /// Cached JSON size of `history`. This is intentionally omitted from the
    /// durable representation and rebuilt when a state is decoded.
    #[serde(skip)]
    pub(crate) history_bytes: usize,
}
impl SessionState {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            status: SessionStatus::Idle,
            active_turn: None,
            history: vec![],
            pending_input: None,
            provider_continuation: None,
            last_seq: EventSeq(0),
            history_bytes: 2,
        }
    }
}
