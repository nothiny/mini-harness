use super::agent_loop::{
    AgentLoop, DEFAULT_TOOL_OUTPUT_LIMIT, bounded_tool_deadline, turn_deadline_for,
};
use crate::{
    durable::EventPayload,
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    runtime::{ExecutionId, ExecutionState, ToolCallId, TurnId},
    tools::ToolContext,
};
use std::sync::Arc;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

impl<P, E, S> AgentLoop<P, E, S>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: crate::durable::EventStore + 'static,
{
    /// Applies a durable approval decision and, when approved, executes the
    /// pending tool before resuming model sampling for the same turn.
    pub async fn approve_tool(
        &mut self,
        turn: TurnId,
        call_id: ToolCallId,
        approved: bool,
        cancel: CancellationToken,
    ) -> Result<(TurnId, String), HarnessError> {
        let active = self
            .state
            .active_turn
            .as_ref()
            .filter(|active| active.turn_id == turn)
            .ok_or_else(|| {
                HarnessError::InvariantViolation("requested turn is not active".into())
            })?;
        if !matches!(
            active.approvals.get(&call_id),
            Some(crate::runtime::ApprovalStatus::Pending)
        ) {
            return Err(HarnessError::InvariantViolation(
                "tool does not have a pending approval".into(),
            ));
        }
        let tool_name =
            active.tool_names.get(&call_id).cloned().ok_or_else(|| {
                HarnessError::InvariantViolation("missing approved tool name".into())
            })?;
        let input = active.tool_inputs.get(&call_id).cloned().ok_or_else(|| {
            HarnessError::InvariantViolation("missing approved tool input".into())
        })?;
        self.append(
            Some(turn),
            EventPayload::ToolApprovalResponded { call_id, approved },
        )
        .await?;
        if !approved {
            self.append(Some(turn), EventPayload::TurnCancelled).await?;
            return Err(HarnessError::Policy(
                format!("approval denied for tool `{}`", tool_name.0).into(),
            ));
        }
        let tool = self
            .tools
            .lookup(&tool_name.0)
            .map_err(|error| HarnessError::Tool(error.to_string().into()))?;
        let execution_id = ExecutionId::new();
        self.append(
            Some(turn),
            EventPayload::ToolStarted {
                call_id,
                execution_id,
            },
        )
        .await?;
        let workspace = self.executor.workspace_root();
        let executor = Arc::clone(&self.executor);
        let output_limit = self
            .config
            .max_tool_result_bytes
            .min(DEFAULT_TOOL_OUTPUT_LIMIT);
        let tool_cancel = cancel.clone();
        let tool_started_at = Instant::now();
        let turn_deadline = turn_deadline_for(
            self.turn_started_at.unwrap_or_else(Instant::now),
            self.config.max_turn_duration,
        );
        let tool_deadline = bounded_tool_deadline(
            tool_started_at,
            turn_deadline,
            self.tool_time_used,
            self.config.max_tool_time,
        );
        let mut task = tokio::spawn(async move {
            tool.execute(
                input,
                ToolContext {
                    executor: executor.as_ref(),
                    cancel: tool_cancel,
                    workspace,
                    tool_call_id: call_id,
                    execution_id,
                    output_limit: crate::runtime::ByteLimit(output_limit),
                },
            )
            .await
        });
        let result = tokio::select! {
            _ = cancel.cancelled() => {
                let cleanup = tokio::time::timeout(
                    super::agent_loop::TOOL_CLEANUP_TIMEOUT,
                    &mut task,
                )
                .await;
                self.record_tool_time(tool_started_at);
                self.append(Some(turn), EventPayload::TurnCancelRequested)
                    .await?;
                match cleanup {
                    Ok(Ok(Ok(value))) if value.len() <= self.config.max_tool_result_bytes => {
                        self.append(
                            Some(turn),
                            EventPayload::ToolCompleted {
                                call_id,
                                result: crate::runtime::ToolResult(value),
                            },
                        )
                        .await?;
                        self.append(Some(turn), EventPayload::TurnCancelled).await?;
                    }
                    Ok(Ok(Ok(_))) => {
                        self.append(
                            Some(turn),
                            EventPayload::ToolFailed {
                                call_id,
                                error: format!(
                                    "tool result exceeds the {} byte limit",
                                    self.config.max_tool_result_bytes
                                )
                                .into(),
                            },
                        )
                        .await?;
                        self.append(Some(turn), EventPayload::TurnCancelled).await?;
                    }
                    Ok(Ok(Err(error))) => {
                        self.append(
                            Some(turn),
                            EventPayload::ToolFailed {
                                call_id,
                                error: error.into(),
                            },
                        )
                        .await?;
                        self.append(Some(turn), EventPayload::TurnCancelled).await?;
                    }
                    Ok(Err(join_error)) => {
                        self.append(
                            Some(turn),
                            EventPayload::ToolOutcomeUnknown {
                                call_id,
                                reason: super::agent_loop::tool_join_error(join_error),
                            },
                        )
                        .await?;
                    }
                    Err(_) => {
                        task.abort();
                        let _ = task.await;
                        self.append(
                            Some(turn),
                            EventPayload::ToolOutcomeUnknown {
                                call_id,
                                reason: "tool cleanup timed out after cancellation".into(),
                            },
                        )
                        .await?;
                    }
                }
                return Err(HarnessError::Cancelled);
            }
            _ = sleep_until(tool_deadline) => {
                task.abort();
                let _ = task.await;
                self.record_tool_time(tool_started_at);
                self.append(
                    Some(turn),
                    EventPayload::ToolOutcomeUnknown {
                        call_id,
                        reason: if tool_deadline < turn_deadline {
                            "cumulative tool time limit exceeded during approved tool execution"
                                .into()
                        } else {
                            "turn wall-clock limit exceeded during approved tool execution".into()
                        },
                    },
                )
                .await?;
                return Err(HarnessError::Timeout);
            }
            result = &mut task => match result {
                Ok(result) => result,
                Err(join_error) => Err(super::agent_loop::tool_join_error(join_error)),
            }
        };
        self.record_tool_time(tool_started_at);
        match result {
            Ok(value) if value.len() <= self.config.max_tool_result_bytes => {
                if let Err(error) = self
                    .append(
                        Some(turn),
                        EventPayload::ToolCompleted {
                            call_id,
                            result: crate::runtime::ToolResult(value),
                        },
                    )
                    .await
                {
                    let message = error.to_string();
                    let _ = self
                        .append(
                            Some(turn),
                            EventPayload::ToolFailed {
                                call_id,
                                error: message.clone().into(),
                            },
                        )
                        .await;
                    let _ = self
                        .append(Some(turn), EventPayload::TurnFailed { error: message })
                        .await;
                    return Err(error);
                }
            }
            Ok(_) => {
                let message = format!(
                    "tool result exceeds the {} byte limit",
                    self.config.max_tool_result_bytes
                );
                self.append(
                    Some(turn),
                    EventPayload::ToolFailed {
                        call_id,
                        error: message.clone().into(),
                    },
                )
                .await?;
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: message.clone(),
                    },
                )
                .await?;
                return Err(HarnessError::Tool(message.into()));
            }
            Err(error) => {
                self.append(
                    Some(turn),
                    EventPayload::ToolFailed {
                        call_id,
                        error: error.clone().into(),
                    },
                )
                .await?;
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: error.clone(),
                    },
                )
                .await?;
                return Err(HarnessError::Tool(error.into()));
            }
        }
        self.continue_turn(turn, cancel).await
    }

    /// Marks running executions unknown before terminating an actor that cannot
    /// wait for an uncooperative tool. A turn with unknown side effects stays
    /// active so startup recovery can require an explicit decision.
    pub async fn cancel_active_after_shutdown(
        &mut self,
        turn: TurnId,
        reason: &str,
    ) -> Result<(), HarnessError> {
        let active = self
            .state
            .active_turn
            .as_ref()
            .filter(|active| active.turn_id == turn)
            .ok_or_else(|| HarnessError::InvariantViolation("requested turn is not active".into()))?
            .executions
            .clone();
        self.append(Some(turn), EventPayload::TurnCancelRequested)
            .await?;
        let running = active
            .iter()
            .filter_map(|(call_id, execution)| {
                matches!(execution, ExecutionState::Running { .. }).then_some(*call_id)
            })
            .collect::<Vec<_>>();
        for call_id in &running {
            self.append(
                Some(turn),
                EventPayload::ToolOutcomeUnknown {
                    call_id: *call_id,
                    reason: reason.to_owned(),
                },
            )
            .await?;
        }
        let had_unknown = active
            .values()
            .any(|execution| matches!(execution, ExecutionState::OutcomeUnknown));
        if self.state.active_turn.is_some() && running.is_empty() && !had_unknown {
            self.append(Some(turn), EventPayload::TurnCancelled).await?;
        }
        Ok(())
    }
}
