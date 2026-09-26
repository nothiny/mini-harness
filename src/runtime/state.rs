use super::{
    ids::{SessionId, ToolCallId, TurnId},
    types::{EventSeq, ExecutionState, ModelText, ToolName, ToolResult, UserInput},
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

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
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Idle,
    Running,
    Failed,
    Closed,
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
    pub executions: HashMap<ToolCallId, ExecutionState>,
    pub tool_names: HashMap<ToolCallId, ToolName>,
    /// Inputs were added after checkpoint schema 1.  Keep the field
    /// deserializable when restoring an old snapshot; the reducer fills it
    /// for requests that are replayed after the snapshot.
    #[serde(default)]
    pub tool_inputs: HashMap<ToolCallId, serde_json::Value>,
    pub approvals: HashMap<ToolCallId, ApprovalStatus>,
    pub final_text: Option<ModelText>,
}

impl TurnState {
    /// Creates an empty running turn. Event replay remains the source of
    /// truth; this helper is for integrations that need a migration-safe
    /// initial value instead of a struct literal.
    pub fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            status: TurnStatus::Running,
            last_model_response: None,
            tool_calls: Vec::new(),
            executions: HashMap::new(),
            tool_names: HashMap::new(),
            tool_inputs: HashMap::new(),
            approvals: HashMap::new(),
            final_text: None,
        }
    }
}
/// Where a session is in the scheduler lifecycle (design §12):
/// `Idle → RunningTurn → WaitingApproval → RunningTool → RunningTurn → Idle`.
///
/// Lives next to the reducer state so the session actor can answer state
/// queries without depending on the scheduler module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRunState {
    Idle,
    RunningTurn { turn_id: TurnId },
    WaitingApproval { turn_id: TurnId },
    RunningTool { turn_id: TurnId },
}

/// Projects reducer state onto [`SessionRunState`]. Approval wins over tool
/// execution because an approved tool keeps running while later calls may
/// still wait for their own approvals.
pub fn run_state(state: &SessionState) -> SessionRunState {
    let Some(turn) = &state.active_turn else {
        return SessionRunState::Idle;
    };
    let waiting_approval = turn
        .approvals
        .iter()
        .any(|(_, status)| matches!(status, ApprovalStatus::Pending));
    if waiting_approval {
        return SessionRunState::WaitingApproval {
            turn_id: turn.turn_id,
        };
    }
    let running_tool = turn
        .executions
        .values()
        .any(|execution| matches!(execution, ExecutionState::Running { .. }));
    if running_tool {
        return SessionRunState::RunningTool {
            turn_id: turn.turn_id,
        };
    }
    SessionRunState::RunningTurn {
        turn_id: turn.turn_id,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HistoryItem {
    User(UserInput),
    Assistant(ModelText),
    Tool {
        call_id: ToolCallId,
        name: ToolName,
        result: ToolResult,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: SessionId,
    pub status: SessionStatus,
    pub active_turn: Option<TurnState>,
    pub history: Vec<HistoryItem>,
    pub pending_inputs: VecDeque<UserInput>,
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
            pending_inputs: VecDeque::new(),
            provider_continuation: None,
            last_seq: EventSeq(0),
            history_bytes: 2,
        }
    }

    pub(crate) fn rebuild_history_bytes(&mut self) -> Result<(), serde_json::Error> {
        self.history_bytes = serde_json::to_vec(&self.history)?.len();
        Ok(())
    }
}
