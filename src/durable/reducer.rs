use super::event::{Event, EventPayload};
use crate::{
    error::{HarnessError, ToolError},
    runtime::{
        state::{ApprovalStatus, HistoryItem, SessionState, SessionStatus, TurnState, TurnStatus},
        types::ExecutionState,
    },
};
/// Durable history is bounded independently of the provider-facing context.
/// Keeping this below the event-log limit prevents one session snapshot from
/// growing without bound while still retaining enough audit history.
pub(crate) const MAX_HISTORY_BYTES: usize = 1024 * 1024;

fn push_history(state: &mut SessionState, item: HistoryItem) -> Result<(), HarnessError> {
    if state.history_bytes == 0 && !state.history.is_empty() {
        state.history_bytes = serde_json::to_vec(&state.history)
            .map_err(|error| HarnessError::InvariantViolation(error.to_string()))?
            .len();
    }
    let item_size = serde_json::to_vec(&item)
        .map_err(|error| HarnessError::InvariantViolation(error.to_string()))?
        .len();
    let size = state
        .history_bytes
        .saturating_add(item_size)
        .saturating_add(if state.history.is_empty() { 0 } else { 1 });
    if size > MAX_HISTORY_BYTES {
        return Err(HarnessError::InvariantViolation(format!(
            "session history exceeds the {MAX_HISTORY_BYTES} byte limit"
        )));
    }
    state.history.push(item);
    state.history_bytes = size;
    Ok(())
}

pub fn reduce(state: &mut SessionState, event: &Event) -> Result<(), HarnessError> {
    if event.session_id != state.session_id {
        return Err(HarnessError::InvariantViolation(
            "event belongs to another session".into(),
        ));
    }
    if event.seq.0 != state.last_seq.0 + 1 {
        return Err(HarnessError::InvariantViolation(format!(
            "expected seq {}, got {}",
            state.last_seq.0 + 1,
            event.seq
        )));
    }
    if state.last_seq.0 == 0 && !matches!(&event.payload, EventPayload::SessionCreated) {
        return Err(HarnessError::InvariantViolation(
            "the first event must create the session".into(),
        ));
    }
    if state.last_seq.0 > 0 && matches!(&event.payload, EventPayload::SessionCreated) {
        return Err(HarnessError::InvariantViolation(
            "session can only be created once".into(),
        ));
    }
    let result = match &event.payload {
        EventPayload::SessionCreated if state.last_seq.0 == 0 && event.turn_id.is_none() => {
            state.status = SessionStatus::Idle;
            Ok(())
        }
        EventPayload::UserInputRecorded { input }
            if state.active_turn.is_some() && event.turn_id.is_none() =>
        {
            // Durable input queue (design §7.4 second phase / §12): the input
            // becomes part of the conversation immediately — so the running
            // turn's next sampling round sees it as a steer — and stays in
            // `pending_inputs` so the next `turn.started` consumes it in
            // order. Nothing is dropped silently.
            push_history(state, HistoryItem::User(input.clone()))?;
            state.pending_inputs.push_back(input.clone());
            Ok(())
        }
        EventPayload::UserInputRecorded { input }
            if state.active_turn.is_none()
                && event.turn_id.is_none()
                && matches!(state.status, SessionStatus::Idle | SessionStatus::Failed) =>
        {
            push_history(state, HistoryItem::User(input.clone()))?;
            state.pending_inputs.push_back(input.clone());
            // A failed turn is terminal for that turn, but does not poison
            // the session forever. Recording a new user input is the
            // explicit recovery transition back to the idle state.
            state.status = SessionStatus::Idle;
            Ok(())
        }
        EventPayload::TurnStarted if state.active_turn.is_none() => {
            let id = event
                .turn_id
                .ok_or_else(|| HarnessError::InvariantViolation("missing turn id".into()))?;
            if state.pending_inputs.pop_front().is_none() {
                return Err(HarnessError::InvariantViolation(
                    "turn started without user input".into(),
                ));
            }
            state.active_turn = Some(TurnState {
                turn_id: id,
                status: TurnStatus::Running,
                last_model_response: None,
                tool_calls: vec![],
                executions: Default::default(),
                tool_names: Default::default(),
                tool_inputs: Default::default(),
                approvals: Default::default(),
                final_text: None,
            });
            state.status = SessionStatus::Running;
            Ok(())
        }
        EventPayload::ModelResponseRecorded { text } => {
            let turn = active_turn_mut(state, event)?;
            turn.last_model_response = Some(text.clone());
            push_history(state, HistoryItem::Assistant(text.clone()))
        }
        EventPayload::ProviderContinuationUpdated { continuation } => {
            let _ = active_turn_mut(state, event)?;
            if continuation.as_ref().is_some_and(|c| {
                c.history_cursor > state.history.len() || c.native_call_ids.len() > 32
            }) {
                return Err(HarnessError::InvariantViolation(
                    "invalid provider continuation".into(),
                ));
            }
            state.provider_continuation = continuation.clone();
            Ok(())
        }
        EventPayload::ProviderAttemptRecorded { .. } => {
            let _ = active_turn_mut(state, event)?;
            Ok(())
        }
        EventPayload::ToolRequested {
            call_id,
            name,
            input,
        } => {
            let turn = active_turn_mut(state, event)?;
            if turn.executions.contains_key(call_id) {
                return Err(HarnessError::InvariantViolation(
                    "duplicate tool request".into(),
                ));
            }
            turn.tool_calls.push(*call_id);
            turn.tool_names.insert(*call_id, name.clone());
            turn.tool_inputs.insert(*call_id, input.clone());
            turn.executions.insert(*call_id, ExecutionState::Pending);
            Ok(())
        }
        EventPayload::ToolStarted {
            call_id,
            execution_id,
        } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Pending) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool started more than once".into(),
                    ));
                }
                None => {
                    return Err(HarnessError::InvariantViolation(
                        "tool started before request".into(),
                    ));
                }
            };
            match turn.approvals.get(call_id) {
                Some(ApprovalStatus::Approved) | None => {}
                Some(ApprovalStatus::Pending) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool started before approval".into(),
                    ));
                }
                Some(ApprovalStatus::Denied) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool started after approval was denied".into(),
                    ));
                }
            }
            turn.executions.insert(
                *call_id,
                ExecutionState::Running {
                    execution_id: *execution_id,
                },
            );
            Ok(())
        }
        EventPayload::ToolApprovalRequested { call_id } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Pending) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "approval requested for a non-pending tool".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            }
            if turn
                .approvals
                .insert(*call_id, ApprovalStatus::Pending)
                .is_some()
            {
                return Err(HarnessError::InvariantViolation(
                    "approval requested more than once".into(),
                ));
            }
            Ok(())
        }
        EventPayload::ToolBatchDeferred {
            call_ids,
            reason: _,
        } => {
            let turn = active_turn_mut(state, event)?;
            if call_ids.is_empty()
                || call_ids
                    .iter()
                    .any(|call_id| turn.executions.contains_key(call_id))
            {
                return Err(HarnessError::InvariantViolation(
                    "invalid deferred tool batch".into(),
                ));
            }
            Ok(())
        }
        EventPayload::ToolApprovalResponded { call_id, approved } => {
            let turn = active_turn_mut(state, event)?;
            match turn.approvals.get(call_id) {
                Some(ApprovalStatus::Pending) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "approval responded more than once".into(),
                    ));
                }
                None => {
                    return Err(HarnessError::InvariantViolation(
                        "approval response without request".into(),
                    ));
                }
            }
            turn.approvals.insert(
                *call_id,
                if *approved {
                    ApprovalStatus::Approved
                } else {
                    ApprovalStatus::Denied
                },
            );
            Ok(())
        }
        EventPayload::ToolPolicyDenied { call_id, reason } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Pending) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "policy denied tool after execution was finalized".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            }
            // A policy denial is not an approval decision: the approvals map
            // is untouched so replay cannot mistake it for a user response.
            turn.executions.insert(
                *call_id,
                ExecutionState::Failed {
                    error: crate::error::ExecutionError::Tool {
                        error: ToolError::from(reason.clone()),
                    },
                },
            );
            Ok(())
        }
        EventPayload::ToolCompleted { call_id, result } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Running { .. }) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool completed without running execution".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            };
            let tool_name = turn
                .tool_names
                .get(call_id)
                .cloned()
                .ok_or_else(|| HarnessError::InvariantViolation("missing tool name".into()))?;
            turn.executions.insert(
                *call_id,
                ExecutionState::Completed {
                    result: result.clone(),
                },
            );
            push_history(
                state,
                HistoryItem::Tool {
                    call_id: *call_id,
                    name: tool_name,
                    result: result.clone(),
                },
            )
        }
        EventPayload::ToolFailed { call_id, error } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Pending | ExecutionState::Running { .. }) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool failed after execution was finalized".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            };
            turn.executions.insert(
                *call_id,
                ExecutionState::Failed {
                    error: crate::error::ExecutionError::Tool {
                        error: error.clone(),
                    },
                },
            );
            Ok(())
        }
        EventPayload::ToolOutcomeUnknown { call_id, reason: _ } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Running { .. }) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool outcome marked unknown without a running execution".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            }
            turn.executions
                .insert(*call_id, ExecutionState::OutcomeUnknown);
            Ok(())
        }
        EventPayload::RecoveryStarted { reason: _ }
        | EventPayload::RecoveryCompleted { summary: _ }
        | EventPayload::CheckpointCreated => {
            if event.turn_id.is_some() {
                return Err(HarnessError::InvariantViolation(
                    "durable lifecycle event cannot belong to a turn".into(),
                ));
            }
            Ok(())
        }
        EventPayload::RecoveryActionRequired { call_id, policy: _ } => {
            if let Some(call_id) = call_id {
                let turn = active_turn_mut(state, event)?;
                match turn.executions.get(call_id) {
                    Some(
                        ExecutionState::Pending
                        | ExecutionState::Running { .. }
                        | ExecutionState::OutcomeUnknown,
                    ) => {}
                    Some(_) => {
                        return Err(HarnessError::InvariantViolation(
                            "recovery action references a completed tool call".into(),
                        ));
                    }
                    None => {
                        return Err(HarnessError::InvariantViolation(
                            "recovery action references an unknown tool call".into(),
                        ));
                    }
                }
            } else {
                if event.turn_id.is_none() {
                    return Err(HarnessError::InvariantViolation(
                        "recovery action without a turn id".into(),
                    ));
                }
                let _ = active_turn_mut(state, event)?;
            }
            Ok(())
        }
        EventPayload::TurnCompleted { text } => {
            let turn = active_turn_mut(state, event)?;
            if turn
                .executions
                .values()
                .any(|execution| !matches!(execution, ExecutionState::Completed { .. }))
            {
                return Err(HarnessError::InvariantViolation(
                    "turn completed with unfinished or failed tool execution".into(),
                ));
            }
            turn.status = TurnStatus::Completed;
            turn.final_text = Some(text.clone());
            let has_matching_model_response = turn.last_model_response.as_ref() == Some(text);
            if !has_matching_model_response {
                push_history(state, HistoryItem::Assistant(text.clone()))?;
            }
            state.active_turn = None;
            state.status = SessionStatus::Idle;
            Ok(())
        }
        EventPayload::TurnFailed { .. } => {
            let turn = active_turn_mut(state, event)?;
            turn.status = TurnStatus::Failed;
            state.active_turn = None;
            state.status = SessionStatus::Failed;
            // A failed turn cannot safely resume the provider cursor on a
            // subsequent user input.  Continuations are scoped to one
            // successful turn and must not leak into recovery sampling.
            state.provider_continuation = None;
            Ok(())
        }
        EventPayload::TurnCancelRequested => {
            let _ = active_turn_mut(state, event)?;
            Ok(())
        }
        EventPayload::TurnCancelled => {
            let turn = active_turn_mut(state, event)?;
            turn.status = TurnStatus::Cancelled;
            state.active_turn = None;
            state.status = SessionStatus::Idle;
            state.provider_continuation = None;
            Ok(())
        }
        EventPayload::TurnTimedOut => {
            let turn = active_turn_mut(state, event)?;
            turn.status = TurnStatus::TimedOut;
            state.active_turn = None;
            state.status = SessionStatus::Idle;
            state.provider_continuation = None;
            Ok(())
        }
        _ => Err(HarnessError::InvariantViolation(
            "invalid event transition".into(),
        )),
    };
    result?;
    state.last_seq = event.seq;
    Ok(())
}
fn active_turn_mut<'a>(
    state: &'a mut SessionState,
    event: &Event,
) -> Result<&'a mut TurnState, HarnessError> {
    let turn = state
        .active_turn
        .as_mut()
        .ok_or_else(|| HarnessError::InvariantViolation("no active turn".into()))?;
    if event.turn_id.as_ref() != Some(&turn.turn_id) {
        return Err(HarnessError::InvariantViolation(
            "event turn mismatch".into(),
        ));
    }
    Ok(turn)
}
