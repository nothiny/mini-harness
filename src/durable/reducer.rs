use super::event::{Event, EventPayload, ToolOutcome};
use crate::{
    error::HarnessError,
    runtime::{
        state::{ApprovalStatus, HistoryItem, SessionState, SessionStatus, TurnState, TurnStatus},
        types::ExecutionState,
    },
};
/// Durable history is bounded independently of the provider-facing context.
///
/// 两层限制各司其职:
///
/// * 这里（8MB）: 累积审计历史的上限，防止一个会话快照无限增长；
/// * `AgentLoopConfig::max_context_bytes`（1MB）: 单次模型请求的上限，
///   作用在 ContextSnapshot 的字节预算截取之后。
///
/// 若两者共用同一个限制，长会话的累积 history 会先触顶，导致截取
/// 后本还很小的请求被误拒。
pub(crate) const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;

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

/// Ends the provider cursor for a failed/cancelled turn without discarding
/// reasoning metadata that a Chat Completions thinking model requires when
/// the normalized assistant history is replayed on the next user turn.
///
/// Responses-style cursors must not survive a terminal turn: they refer to
/// an incomplete provider exchange. The reasoning sequence and native ids
/// are independent replay metadata, so retain them with a replay-only
/// continuation. This keeps the next request from reusing a stale response
/// id while still satisfying Chat Completions' replay contract for prior tool
/// messages.
fn reset_provider_cursor(state: &mut SessionState) {
    let Some(mut continuation) = state.provider_continuation.take() else {
        return;
    };
    let has_replay_metadata = !continuation.native_call_ids.is_empty()
        || continuation.reasoning_content.is_some()
        || continuation.reasoning_history.iter().any(Option::is_some);
    if !has_replay_metadata {
        return;
    }
    continuation.response_id = None;
    continuation.history_cursor = 0;
    state.provider_continuation = Some(continuation);
}

/// 确定性 Reducer: 从事件流重建完整状态。
///
/// **核心不变量**: 相同的事件序列 → 相同的状态（无论重放多少次）。
///
/// 这是 Event Sourcing 的核心：状态是事件的"折叠"(fold)。
///
/// # 为什么需要确定性？
///
/// 1. **崩溃恢复**: 进程崩溃后，从事件日志重放即可恢复到崩溃前状态
/// 2. **时间旅行**: 可以重放到任意历史时刻（`mini-harness inspect
///    --until <seq>` 只重放前 N 条事件），用于调试
/// 3. **一致性**: 多个进程读取同一事件日志，得到相同的状态视图
///
/// # 如何保证确定性？
///
/// - ❌ 不使用 `Instant::now()`（时间是外部输入）
/// - ❌ 不使用 `rand::random()`（随机数是不可重现的）
/// - ✅ 所有时间戳来自事件本身的 `timestamp` 字段
/// - ✅ 所有随机性来自模型响应（已记录在事件中）
///
/// # 非法转换直接报错
///
/// Reducer 不修复坏日志，而是失败 —— 让调用方决定如何处置。
/// 例如: `tool.completed` 引用一个不存在的 call_id、事件序号断裂、
/// turn 未结束就开启新 turn 等。
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
        // 简化版没有 durable queue: turn 运行中（或还有未消费的输入时）
        // 记录新输入是非法转换，由 AgentLoop 在追加前直接拒绝。
        EventPayload::UserInputRecorded { input }
            if state.active_turn.is_none()
                && state.pending_input.is_none()
                && event.turn_id.is_none()
                && matches!(state.status, SessionStatus::Idle | SessionStatus::Failed) =>
        {
            push_history(state, HistoryItem::User(input.clone()))?;
            state.pending_input = Some(input.clone());
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
            if state.pending_input.take().is_none() {
                return Err(HarnessError::InvariantViolation(
                    "turn started without user input".into(),
                ));
            }
            state.active_turn = Some(TurnState::new(id));
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
            requires_approval,
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
            if *requires_approval {
                turn.approvals.insert(*call_id, ApprovalStatus::Pending);
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
                        "approval response for a tool that never asked".into(),
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
        EventPayload::ToolCompleted {
            call_id,
            outcome,
            duration_ms: _,
        } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Pending) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool completed after execution was finalized".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            };
            // 带审批的调用必须先收到用户的决定才能执行。
            if matches!(turn.approvals.get(call_id), Some(ApprovalStatus::Pending)) {
                return Err(HarnessError::InvariantViolation(
                    "tool completed while approval is still pending".into(),
                ));
            }
            // 在副作用可能发生前就被拒绝的调用（校验失败）允许模型在
            // 后续步骤中重试；执行后的失败则阻塞 turn 完成。
            if matches!(outcome, ToolOutcome::Rejected { .. }) {
                turn.rejected_tool_calls.insert(*call_id);
            }
            let tool_name = turn
                .tool_names
                .get(call_id)
                .cloned()
                .ok_or_else(|| HarnessError::InvariantViolation("missing tool name".into()))?;
            let tool_input = turn.tool_inputs.get(call_id).cloned().unwrap_or_default();
            turn.executions.insert(
                *call_id,
                match outcome {
                    ToolOutcome::Succeeded { result } => ExecutionState::Completed {
                        result: result.clone(),
                    },
                    ToolOutcome::Rejected { error } | ToolOutcome::Failed { error } => {
                        ExecutionState::Failed {
                            error: crate::error::ExecutionError::Tool {
                                error: error.clone(),
                            },
                        }
                    }
                },
            );
            push_history(
                state,
                HistoryItem::Tool {
                    call_id: *call_id,
                    name: tool_name,
                    input: tool_input,
                    result: outcome.as_tool_result(),
                },
            )
        }
        EventPayload::ToolOutcomeUnknown { call_id, reason: _ } => {
            let turn = active_turn_mut(state, event)?;
            match turn.executions.get(call_id) {
                Some(ExecutionState::Pending) => {}
                Some(_) => {
                    return Err(HarnessError::InvariantViolation(
                        "tool outcome marked unknown after execution was finalized".into(),
                    ));
                }
                None => return Err(HarnessError::InvariantViolation("unknown tool call".into())),
            };
            turn.executions
                .insert(*call_id, ExecutionState::OutcomeUnknown);
            Ok(())
        }
        EventPayload::TurnCompleted { text } => {
            let turn = active_turn_mut(state, event)?;
            if turn.executions.iter().any(|(call_id, execution)| {
                !(matches!(execution, ExecutionState::Completed { .. })
                    || matches!(execution, ExecutionState::Failed { .. })
                        && turn.rejected_tool_calls.contains(call_id))
            }) {
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
            // subsequent user input. Preserve replayable reasoning and native
            // tool ids, while reset_provider_cursor removes the response id
            // that belongs to the incomplete exchange.
            reset_provider_cursor(state);
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
            reset_provider_cursor(state);
            Ok(())
        }
        EventPayload::TurnTimedOut => {
            let turn = active_turn_mut(state, event)?;
            turn.status = TurnStatus::TimedOut;
            state.active_turn = None;
            state.status = SessionStatus::Idle;
            reset_provider_cursor(state);
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
