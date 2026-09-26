use crate::{
    error::{DurableError, ToolError},
    runtime::{
        ids::*,
        types::{EventSeq, ModelText, RecoveryPolicy, ToolName, ToolResult, UserInput},
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
    ToolRequested {
        call_id: ToolCallId,
        name: ToolName,
        input: serde_json::Value,
    },
    ToolStarted {
        call_id: ToolCallId,
        execution_id: ExecutionId,
    },
    ToolApprovalRequested {
        call_id: ToolCallId,
    },
    /// Records tool calls from a provider batch that were deferred because an
    /// earlier call is waiting for approval.
    ToolBatchDeferred {
        call_ids: Vec<ToolCallId>,
        reason: String,
    },
    ToolApprovalResponded {
        call_id: ToolCallId,
        approved: bool,
    },
    /// Records that the policy layer denied a tool call before execution.
    ///
    /// This is deliberately separate from [`ToolApprovalResponded`]: a
    /// policy denial is a harness decision, while an approval response is a
    /// user decision. Folding them into one event would make it impossible
    /// to tell from the log whether a human was ever asked.
    ToolPolicyDenied {
        call_id: ToolCallId,
        reason: String,
    },
    ToolCompleted {
        call_id: ToolCallId,
        result: ToolResult,
    },
    ToolFailed {
        call_id: ToolCallId,
        error: ToolError,
    },
    ToolOutcomeUnknown {
        call_id: ToolCallId,
        reason: String,
    },
    RecoveryStarted {
        reason: String,
    },
    RecoveryActionRequired {
        call_id: Option<ToolCallId>,
        policy: RecoveryPolicy,
    },
    RecoveryCompleted {
        summary: String,
    },
    CheckpointCreated,
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

impl EventPayload {
    /// Stable, human-readable event name shared by tracing, the JSONL
    /// protocol projection, and diagnostics. Matches the names in
    /// `docs/design.md` §11.2.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::UserInputRecorded { .. } => "user.input.recorded",
            Self::TurnStarted => "turn.started",
            Self::ModelResponseRecorded { .. } => "model.response.recorded",
            Self::ProviderContinuationUpdated { .. } => "provider.continuation.updated",
            Self::ProviderAttemptRecorded { .. } => "provider.attempt.recorded",
            Self::ToolRequested { .. } => "tool.requested",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolApprovalRequested { .. } => "tool.approval.requested",
            Self::ToolBatchDeferred { .. } => "tool.batch.deferred",
            Self::ToolApprovalResponded { .. } => "tool.approval.responded",
            Self::ToolPolicyDenied { .. } => "tool.policy_denied",
            Self::ToolCompleted { .. } => "tool.completed",
            Self::ToolFailed { .. } => "tool.failed",
            Self::ToolOutcomeUnknown { .. } => "tool.outcome_unknown",
            Self::RecoveryStarted { .. } => "recovery.started",
            Self::RecoveryActionRequired { .. } => "recovery.action_required",
            Self::RecoveryCompleted { .. } => "recovery.completed",
            Self::CheckpointCreated => "checkpoint.created",
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
    pub const CURRENT_SCHEMA_VERSION: u32 = 2;
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
        // Version 2 only adds payload variants. Existing version 1 events
        // remain readable so persisted logs can be replayed in place. A
        // caller that needs a single-version log can use
        // `Event::migrate_to_current` (or the event-store helper).
        if !matches!(event.schema_version, 1 | Self::CURRENT_SCHEMA_VERSION) {
            return Err(DurableError::UnsupportedSchema(event.schema_version));
        }
        Ok(event)
    }

    /// Upgrade a decoded legacy event to the current durable schema.
    ///
    /// Event payloads are intentionally unchanged: schema 1 and schema 2
    /// share the original payload representation, while schema 2 adds new
    /// variants for events emitted by newer runtimes.  Logs containing those
    /// newer variants must therefore be consumed by a schema 2 reader before
    /// being migrated.  Keeping this operation explicit prevents a mixed log
    /// from being silently rewritten while it is still being inspected.
    pub fn migrate_to_current(mut self) -> Result<Self, DurableError> {
        if !matches!(self.schema_version, 1 | Self::CURRENT_SCHEMA_VERSION) {
            return Err(DurableError::UnsupportedSchema(self.schema_version));
        }
        self.schema_version = Self::CURRENT_SCHEMA_VERSION;
        Ok(self)
    }
}
