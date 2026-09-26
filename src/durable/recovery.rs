//! Recovery inspection for event logs that end in the middle of a turn.
//!
//! Recovery never executes a tool.  It replays the durable facts and reports the
//! actions that a caller must take before deciding whether a turn can continue.

use super::{
    event::{Event, EventPayload},
    reduce,
    store::EventStore,
};
use crate::{
    error::HarnessError,
    runtime::{
        ids::{ExecutionId, SessionId, ToolCallId, TurnId},
        state::{ApprovalStatus, SessionState},
        types::{EventSeq, ExecutionState, RecoveryPolicy, ToolName},
    },
};
use serde::{Deserialize, Serialize};

/// A fact that requires an explicit recovery decision after replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RecoveryFinding {
    /// The log contains a turn that did not reach a terminal event.
    ActiveTurn { turn_id: TurnId },
    /// The model request may have been in flight when the process stopped.
    PendingModelResponse { turn_id: TurnId },
    /// A tool was approved but its execution did not finish.
    PendingApproval {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: ToolName,
    },
    /// A tool request was durable, but execution had not started yet.
    PendingTool {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: ToolName,
        policy: RecoveryPolicy,
    },
    /// A tool may have caused an external side effect without a durable result.
    UnknownTool {
        turn_id: TurnId,
        call_id: ToolCallId,
        tool_name: ToolName,
        execution_id: Option<ExecutionId>,
        policy: RecoveryPolicy,
    },
}

/// The replayed state together with all recovery findings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub session_id: SessionId,
    pub last_seq: EventSeq,
    pub state: SessionState,
    pub findings: Vec<RecoveryFinding>,
}

impl RecoveryReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    pub fn action_required(&self) -> bool {
        self.findings.iter().any(|finding| {
            matches!(
                finding,
                RecoveryFinding::ActiveTurn { .. }
                    | RecoveryFinding::PendingApproval { .. }
                    | RecoveryFinding::PendingTool { .. }
                    | RecoveryFinding::PendingModelResponse { .. }
                    | RecoveryFinding::UnknownTool { .. }
            )
        })
    }
}

/// Replays a complete event slice and classifies any unfinished work.
pub fn classify_events(
    session_id: SessionId,
    events: &[Event],
) -> Result<RecoveryReport, HarnessError> {
    if events.is_empty() {
        return Err(HarnessError::InvariantViolation(
            "cannot inspect an empty event log".into(),
        ));
    }
    let mut state = SessionState::new(session_id);
    for event in events {
        reduce(&mut state, event)?;
    }
    Ok(classify_state(state))
}

/// Replays a state that was already reconstructed by a caller.
pub fn classify_state(state: SessionState) -> RecoveryReport {
    let mut findings = Vec::new();
    if let Some(turn) = &state.active_turn {
        findings.push(RecoveryFinding::ActiveTurn {
            turn_id: turn.turn_id,
        });
        if turn.last_model_response.is_none() {
            findings.push(RecoveryFinding::PendingModelResponse {
                turn_id: turn.turn_id,
            });
        }
        for call_id in &turn.tool_calls {
            let Some(tool_name) = turn.tool_names.get(call_id) else {
                continue;
            };
            if matches!(turn.approvals.get(call_id), Some(ApprovalStatus::Pending)) {
                findings.push(RecoveryFinding::PendingApproval {
                    turn_id: turn.turn_id,
                    call_id: *call_id,
                    tool_name: tool_name.clone(),
                });
                continue;
            }
            let Some(execution) = turn.executions.get(call_id) else {
                continue;
            };
            let execution_id = match execution {
                ExecutionState::Running { execution_id } => Some(*execution_id),
                ExecutionState::OutcomeUnknown => None,
                ExecutionState::Pending => {
                    findings.push(RecoveryFinding::PendingTool {
                        turn_id: turn.turn_id,
                        call_id: *call_id,
                        tool_name: tool_name.clone(),
                        policy: policy_for_tool(tool_name),
                    });
                    continue;
                }
                _ => continue,
            };
            findings.push(RecoveryFinding::UnknownTool {
                turn_id: turn.turn_id,
                call_id: *call_id,
                tool_name: tool_name.clone(),
                execution_id,
                policy: policy_for_tool(tool_name),
            });
        }
    }
    RecoveryReport {
        session_id: state.session_id,
        last_seq: state.last_seq,
        state,
        findings,
    }
}

/// Reads and classifies a durable event store without running any recovery action.
pub async fn inspect_store<S: EventStore + ?Sized>(
    store: &S,
    session_id: SessionId,
) -> Result<RecoveryReport, HarnessError> {
    let events = store.read_from(EventSeq(1)).await?;
    classify_events(session_id, &events)
}

/// Selects the conservative default action for an interrupted tool.
pub fn policy_for_tool(tool_name: &ToolName) -> RecoveryPolicy {
    match tool_name.0.as_str() {
        "read" => RecoveryPolicy::RetryRead,
        "edit" => RecoveryPolicy::InspectEditHash,
        _ => RecoveryPolicy::RequireUserDecision,
    }
}

/// Records conservative recovery facts without executing any interrupted tool.
pub async fn recover_store<S: EventStore + ?Sized>(
    store: &S,
    session_id: SessionId,
) -> Result<RecoveryReport, HarnessError> {
    let mut events = store.read_from(EventSeq(1)).await?;
    let initial = classify_events(session_id, &events)?;
    if initial.findings.is_empty() {
        return Ok(initial);
    }
    if initial
        .state
        .active_turn
        .as_ref()
        .is_some_and(|turn| recovery_completed_for_turn(&events, turn.turn_id))
    {
        return Ok(initial);
    }

    if !recovery_batch_is_open(&events) {
        let event = append_recovery_event(
            store,
            session_id,
            EventPayload::RecoveryStarted {
                reason: "startup recovery classified an unfinished turn".into(),
            },
            None,
        )
        .await?;
        events.push(event);
    }
    for finding in &initial.findings {
        match finding {
            RecoveryFinding::UnknownTool {
                turn_id,
                call_id,
                execution_id: Some(_),
                ..
            } => {
                let event = append_recovery_event(
                    store,
                    session_id,
                    EventPayload::ToolOutcomeUnknown {
                        call_id: *call_id,
                        reason: "harness stopped before the tool result was durable".into(),
                    },
                    Some(*turn_id),
                )
                .await?;
                events.push(event);
                append_action_if_missing(store, session_id, &mut events, finding).await?;
            }
            RecoveryFinding::UnknownTool {
                execution_id: None, ..
            }
            | RecoveryFinding::PendingTool { .. }
            | RecoveryFinding::PendingApproval { .. }
            | RecoveryFinding::PendingModelResponse { .. } => {
                append_action_if_missing(store, session_id, &mut events, finding).await?;
            }
            RecoveryFinding::ActiveTurn { .. } => {
                if initial
                    .findings
                    .iter()
                    .all(|finding| matches!(finding, RecoveryFinding::ActiveTurn { .. }))
                {
                    append_action_if_missing(store, session_id, &mut events, finding).await?;
                }
            }
        }
    }
    let _ = append_recovery_event(
        store,
        session_id,
        EventPayload::RecoveryCompleted {
            summary: "unfinished work marked for explicit recovery action".into(),
        },
        None,
    )
    .await?;
    inspect_store(store, session_id).await
}

/// Abandons an active turn after the caller has inspected its recovery report.
pub async fn abandon_turn<S: EventStore + ?Sized>(
    store: &S,
    session_id: SessionId,
    turn_id: TurnId,
) -> Result<(), HarnessError> {
    let report = inspect_store(store, session_id).await?;
    let Some(turn) = report.state.active_turn else {
        return Err(HarnessError::InvariantViolation(
            "cannot abandon a turn when no turn is active".into(),
        ));
    };
    if turn.turn_id != turn_id {
        return Err(HarnessError::InvariantViolation(
            "requested turn is not the active turn".into(),
        ));
    }
    append_recovery_event(
        store,
        session_id,
        EventPayload::TurnFailed {
            error: "turn abandoned by explicit recovery command".into(),
        },
        Some(turn_id),
    )
    .await?;
    Ok(())
}

async fn append_recovery_event<S: EventStore + ?Sized>(
    store: &S,
    session_id: SessionId,
    payload: EventPayload,
    turn_id: Option<TurnId>,
) -> Result<Event, HarnessError> {
    store
        .append(Event::new(session_id, turn_id, payload))
        .await
        .map_err(HarnessError::from)
}

async fn append_action_if_missing<S: EventStore + ?Sized>(
    store: &S,
    session_id: SessionId,
    events: &mut Vec<Event>,
    finding: &RecoveryFinding,
) -> Result<(), HarnessError> {
    let (turn_id, call_id, policy) = match finding {
        RecoveryFinding::UnknownTool {
            turn_id,
            call_id,
            policy,
            ..
        } => (*turn_id, Some(*call_id), policy.clone()),
        RecoveryFinding::PendingApproval {
            turn_id, call_id, ..
        } => (
            *turn_id,
            Some(*call_id),
            RecoveryPolicy::RequireUserDecision,
        ),
        RecoveryFinding::PendingTool {
            turn_id,
            call_id,
            policy,
            ..
        } => (*turn_id, Some(*call_id), policy.clone()),
        RecoveryFinding::PendingModelResponse { turn_id } => {
            (*turn_id, None, RecoveryPolicy::RequireUserDecision)
        }
        RecoveryFinding::ActiveTurn { turn_id } => {
            (*turn_id, None, RecoveryPolicy::RequireUserDecision)
        }
    };
    let already_recorded = events.iter().any(|event| {
        event.turn_id == Some(turn_id)
            && matches!(
                &event.payload,
                EventPayload::RecoveryActionRequired {
                    call_id: event_call_id,
                    policy: event_policy,
                } if *event_call_id == call_id && *event_policy == policy
            )
    });
    if already_recorded {
        return Ok(());
    }
    let event = append_recovery_event(
        store,
        session_id,
        EventPayload::RecoveryActionRequired { call_id, policy },
        Some(turn_id),
    )
    .await?;
    events.push(event);
    Ok(())
}

fn recovery_batch_is_open(events: &[Event]) -> bool {
    let last_started = events
        .iter()
        .rposition(|event| matches!(&event.payload, EventPayload::RecoveryStarted { .. }));
    let last_completed = events
        .iter()
        .rposition(|event| matches!(&event.payload, EventPayload::RecoveryCompleted { .. }));
    last_started.is_some_and(|started| last_completed.is_none_or(|completed| started > completed))
}

fn recovery_completed_for_turn(events: &[Event], turn_id: TurnId) -> bool {
    let Some(completed) = events
        .iter()
        .rfind(|event| matches!(&event.payload, EventPayload::RecoveryCompleted { .. }))
    else {
        return false;
    };
    let Some(turn_started) = events.iter().rfind(|event| {
        event.turn_id == Some(turn_id) && matches!(&event.payload, EventPayload::TurnStarted)
    }) else {
        return false;
    };
    completed.seq >= turn_started.seq
}
