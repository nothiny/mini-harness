use crate::{
    durable::{Event, EventPayload, EventStore, reduce},
    error::{HarnessError, ProviderError},
    executor::Executor,
    model::{ModelProvider, ModelRequest, ModelResponse},
    policy::{PolicyDecision, ToolPolicy},
    runtime::{
        context::{ContextSnapshot, MAX_CONTEXT_ITEM_BYTES},
        ids::TurnId,
        state::SessionState,
        types::EventSeq,
    },
    tools::{ToolContext, ToolRegistry},
};
use std::{collections::HashSet, sync::Arc};
use tokio::task::JoinError;
use tokio::time::{Duration, Instant, sleep, sleep_until};
use tokio_util::sync::CancellationToken;

pub(super) const DEFAULT_TOOL_OUTPUT_LIMIT: usize = 200_000;
pub(super) const TOOL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Resource limits applied to one turn and to every durable event it emits.
///
/// Duration limits use zero to mean "disabled": `max_turn_duration = 0`
/// imposes no wall-clock budget and `max_tool_time = 0` imposes no cumulative
/// tool-time budget. Count limits keep the natural meaning of zero (a
/// `max_steps` of zero fails the first step, a `max_batch_tool_calls` of zero
/// rejects every batch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLoopConfig {
    pub max_steps: usize,
    pub max_turn_duration: Duration,
    pub max_tool_time: Duration,
    /// Maximum number of tool calls accepted from one provider response.
    /// Larger batches fail the turn before any `tool.requested` event is
    /// appended, so a misbehaving provider cannot grow per-turn state without
    /// bound.
    pub max_batch_tool_calls: usize,
    /// Maximum inputs accepted while a turn is running (the durable queue,
    /// design §12). Rejections happen before anything is appended, so a
    /// refused input is never acknowledged or silently dropped.
    pub max_queued_inputs: usize,
    pub max_context_bytes: usize,
    pub max_event_bytes: usize,
    pub max_tool_result_bytes: usize,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 32,
            max_turn_duration: Duration::from_secs(10 * 60),
            max_tool_time: Duration::from_secs(5 * 60),
            max_batch_tool_calls: DEFAULT_MAX_BATCH_TOOL_CALLS,
            max_queued_inputs: DEFAULT_MAX_QUEUED_INPUTS,
            max_context_bytes: 1024 * 1024,
            max_event_bytes: 1024 * 1024,
            max_tool_result_bytes: DEFAULT_TOOL_OUTPUT_LIMIT,
        }
    }
}

pub(super) const DEFAULT_MAX_BATCH_TOOL_CALLS: usize = 16;
pub(super) const DEFAULT_MAX_QUEUED_INPUTS: usize = 16;

/// Sentinel for "no deadline". A far-future instant keeps deadline arithmetic
/// in `select!` arms simple instead of threading `Option<Instant>` through
/// every call site.
const UNLIMITED_DURATION: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Clone, Copy)]
pub(super) enum CancellationReason {
    Cancelled,
    TimedOut,
}

impl CancellationReason {
    pub(super) fn event(self) -> EventPayload {
        match self {
            Self::Cancelled => EventPayload::TurnCancelled,
            Self::TimedOut => EventPayload::TurnTimedOut,
        }
    }
}

pub(super) fn bounded_tool_deadline(
    started_at: Instant,
    turn_deadline: Instant,
    used: Duration,
    limit: Duration,
) -> Instant {
    if limit.is_zero() {
        // Zero disables the cumulative tool-time budget; only the turn
        // deadline (possibly itself unlimited) still applies.
        return turn_deadline;
    }
    let tool_deadline = started_at + limit.saturating_sub(used);
    if tool_deadline < turn_deadline {
        tool_deadline
    } else {
        turn_deadline
    }
}

/// Turns the wall-clock limit into a concrete deadline. Zero disables the
/// limit and yields the far-future sentinel.
pub(super) fn turn_deadline_for(started_at: Instant, limit: Duration) -> Instant {
    let effective = if limit.is_zero() {
        UNLIMITED_DURATION
    } else {
        limit.min(UNLIMITED_DURATION)
    };
    started_at + effective
}

pub struct AgentLoop<P, E, S> {
    pub provider: Arc<P>,
    pub executor: Arc<E>,
    pub tools: Arc<ToolRegistry>,
    pub store: Arc<S>,
    pub state: SessionState,
    pub system_instructions: Option<String>,
    pub policy: Arc<dyn ToolPolicy>,
    pub config: AgentLoopConfig,
    pub(crate) turn_started_at: Option<Instant>,
    pub(crate) turn_steps: usize,
    pub(crate) tool_time_used: Duration,
}
impl<P, E, S> AgentLoop<P, E, S>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    pub(super) async fn append(
        &mut self,
        turn: Option<TurnId>,
        payload: EventPayload,
    ) -> Result<(), HarnessError> {
        let next_seq =
            EventSeq(self.state.last_seq.0.checked_add(1).ok_or_else(|| {
                HarnessError::InvariantViolation("event sequence overflow".into())
            })?);
        if self.store.last_seq().await? != self.state.last_seq {
            return Err(HarnessError::InvariantViolation(
                "event store and session state are out of sync".into(),
            ));
        }
        let mut event = Event::new(self.state.session_id, turn, payload);
        event.seq = next_seq;
        let encoded = serde_json::to_vec(&event)
            .map_err(|error| HarnessError::InvariantViolation(error.to_string()))?;
        if encoded.len() > self.config.max_event_bytes {
            return Err(HarnessError::InvariantViolation(format!(
                "event exceeds the {} byte limit",
                self.config.max_event_bytes
            )));
        }
        let mut next_state = self.state.clone();
        reduce(&mut next_state, &event)?;
        let history_size = serde_json::to_vec(&next_state.history)
            .map_err(|error| HarnessError::InvariantViolation(error.to_string()))?
            .len();
        if history_size > self.config.max_context_bytes {
            return Err(HarnessError::InvariantViolation(format!(
                "session history exceeds the {} byte context limit",
                self.config.max_context_bytes
            )));
        }
        let persisted = self.store.append(event).await?;
        tracing::debug!(
            target: "mini_harness::durable",
            seq = persisted.seq.0,
            kind = persisted.payload.kind(),
            session_id = %persisted.session_id,
            "event persisted"
        );
        if persisted.seq != next_seq {
            if let Ok(events) = self.store.read_from(EventSeq(1)).await {
                let mut recovered = SessionState::new(self.state.session_id);
                if events
                    .iter()
                    .all(|event| reduce(&mut recovered, event).is_ok())
                {
                    self.state = recovered;
                }
            }
            return Err(HarnessError::InvariantViolation(
                "event store assigned an unexpected sequence".into(),
            ));
        }
        self.state = next_state;
        Ok(())
    }

    pub(super) async fn append_cancellation(
        &mut self,
        turn: TurnId,
        reason: CancellationReason,
    ) -> Result<(), HarnessError> {
        if matches!(reason, CancellationReason::Cancelled) {
            self.append(Some(turn), EventPayload::TurnCancelRequested)
                .await?;
        }
        self.append(Some(turn), reason.event()).await
    }
    pub async fn run_turn(&mut self, input: String) -> Result<(TurnId, String), HarnessError> {
        self.run_turn_with_cancel(input, CancellationToken::new())
            .await
    }
    pub async fn run_turn_with_cancel(
        &mut self,
        input: String,
        cancel: CancellationToken,
    ) -> Result<(TurnId, String), HarnessError> {
        self.run_turn_with_reason(TurnId::new(), input, cancel, CancellationReason::Cancelled)
            .await
    }

    pub async fn run_turn_with_timeout(
        &mut self,
        input: String,
        timeout: Duration,
    ) -> Result<(TurnId, String), HarnessError> {
        let cancel = CancellationToken::new();
        let run = self.run_turn_with_reason(
            TurnId::new(),
            input,
            cancel.clone(),
            CancellationReason::TimedOut,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => result,
            _ = sleep(timeout) => {
                cancel.cancel();
                match (&mut run).await {
                    Err(HarnessError::Cancelled) => Err(HarnessError::Timeout),
                    Err(error) => Err(error),
                    Ok(result) => Ok(result),
                }
            }
        }
    }
    pub async fn run_turn_with_id(
        &mut self,
        turn: TurnId,
        input: String,
        cancel: CancellationToken,
    ) -> Result<(TurnId, String), HarnessError> {
        self.run_turn_with_reason(turn, input, cancel, CancellationReason::Cancelled)
            .await
    }

    async fn run_turn_with_reason(
        &mut self,
        turn: TurnId,
        input: String,
        cancel: CancellationToken,
        cancellation_reason: CancellationReason,
    ) -> Result<(TurnId, String), HarnessError> {
        self.begin_turn(turn, input).await?;
        self.continue_turn_with_reason(turn, cancel, cancellation_reason)
            .await
    }
    pub async fn begin_turn(&mut self, turn: TurnId, input: String) -> Result<(), HarnessError> {
        if self.state.active_turn.is_some() {
            return Err(HarnessError::TurnAlreadyActive);
        }
        if input.len() > MAX_CONTEXT_ITEM_BYTES {
            return Err(HarnessError::InvariantViolation(format!(
                "user input exceeds the {MAX_CONTEXT_ITEM_BYTES} byte context-item limit"
            )));
        }
        if self.state.last_seq.0 == 0 {
            self.append(None, EventPayload::SessionCreated).await?;
        }
        self.append(
            None,
            EventPayload::UserInputRecorded {
                input: crate::runtime::UserInput(input),
            },
        )
        .await?;
        self.begin_queued_turn(turn).await
    }

    /// Records an input durably while a turn is running (durable queue,
    /// design §12). The input becomes visible to the running turn's next
    /// sampling round and is consumed in order by a later
    /// [`Self::begin_queued_turn`].
    pub async fn queue_input(&mut self, input: String) -> Result<(), HarnessError> {
        if input.len() > MAX_CONTEXT_ITEM_BYTES {
            return Err(HarnessError::InvariantViolation(format!(
                "user input exceeds the {MAX_CONTEXT_ITEM_BYTES} byte context-item limit"
            )));
        }
        if self.state.last_seq.0 == 0 {
            self.append(None, EventPayload::SessionCreated).await?;
        }
        self.append(
            None,
            EventPayload::UserInputRecorded {
                input: crate::runtime::UserInput(input),
            },
        )
        .await
    }

    /// Starts a turn for an input that was already recorded durably
    /// (append `turn.started`, consuming the front of `pending_inputs`).
    pub async fn begin_queued_turn(&mut self, turn: TurnId) -> Result<(), HarnessError> {
        if self.state.active_turn.is_some() {
            return Err(HarnessError::TurnAlreadyActive);
        }
        if self.state.pending_inputs.is_empty() {
            return Err(HarnessError::InvariantViolation(
                "turn started without user input".into(),
            ));
        }
        self.append(Some(turn), EventPayload::TurnStarted).await?;
        self.turn_started_at = Some(Instant::now());
        self.turn_steps = 0;
        self.tool_time_used = Duration::ZERO;
        tracing::info!(target: "mini_harness::turn", turn_id = %turn, "queued turn started");
        Ok(())
    }
    pub async fn cancel_turn(&mut self, turn: TurnId) -> Result<(), HarnessError> {
        if self
            .state
            .active_turn
            .as_ref()
            .is_none_or(|active| active.turn_id != turn)
        {
            return Err(HarnessError::InvariantViolation(
                "requested turn is not active".into(),
            ));
        }
        self.append(Some(turn), EventPayload::TurnCancelRequested)
            .await?;
        self.append(Some(turn), EventPayload::TurnCancelled).await
    }

    pub(super) fn record_tool_time(&mut self, started_at: Instant) {
        self.tool_time_used = self.tool_time_used.saturating_add(started_at.elapsed());
    }

    pub async fn continue_turn(
        &mut self,
        turn: TurnId,
        cancel: CancellationToken,
    ) -> Result<(TurnId, String), HarnessError> {
        self.continue_turn_with_reason(turn, cancel, CancellationReason::Cancelled)
            .await
    }

    async fn continue_turn_with_reason(
        &mut self,
        turn: TurnId,
        cancel: CancellationToken,
        cancellation_reason: CancellationReason,
    ) -> Result<(TurnId, String), HarnessError> {
        let turn_started_at = self.turn_started_at.unwrap_or_else(Instant::now);
        let turn_deadline = turn_deadline_for(turn_started_at, self.config.max_turn_duration);
        let mut steps = self.turn_steps;
        loop {
            if cancel.is_cancelled() {
                self.append_cancellation(turn, cancellation_reason).await?;
                return Err(HarnessError::Cancelled);
            }
            if steps >= self.config.max_steps {
                let error = ProviderError::from(format!(
                    "turn exceeded the {} step limit",
                    self.config.max_steps
                ));
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: error.to_string(),
                    },
                )
                .await?;
                return Err(HarnessError::Provider(error));
            }
            if !self.config.max_turn_duration.is_zero()
                && turn_started_at.elapsed() >= self.config.max_turn_duration
            {
                self.append(Some(turn), EventPayload::TurnTimedOut).await?;
                return Err(HarnessError::Timeout);
            }
            if !self.config.max_tool_time.is_zero()
                && self.tool_time_used >= self.config.max_tool_time
            {
                self.append(Some(turn), EventPayload::TurnTimedOut).await?;
                return Err(HarnessError::Timeout);
            }
            steps += 1;
            self.turn_steps = steps;
            let snapshot = ContextSnapshot::from_state(&self.state, self.tools.specs());
            let snapshot = match self.system_instructions.clone() {
                Some(instructions) => snapshot.with_system_instructions(instructions),
                None => snapshot,
            };
            let request: ModelRequest = snapshot.into_model_request();
            let request_size = serde_json::to_vec(&request)
                .map_err(|error| HarnessError::InvariantViolation(error.to_string()))?
                .len();
            if request_size > self.config.max_context_bytes {
                let error = ProviderError::from(format!(
                    "model context exceeds the {} byte limit",
                    self.config.max_context_bytes
                ));
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: error.to_string(),
                    },
                )
                .await?;
                return Err(HarnessError::Provider(error));
            }
            let completion = self
                .sample_model(
                    request,
                    turn,
                    cancel.clone(),
                    turn_deadline,
                    cancellation_reason,
                )
                .await?;
            if completion.continuation != self.state.provider_continuation {
                self.append(
                    Some(turn),
                    EventPayload::ProviderContinuationUpdated {
                        continuation: completion.continuation,
                    },
                )
                .await?;
            }
            let (calls, response_text) = match completion.response {
                ModelResponse::Text(text) => {
                    if let Err(error) = self
                        .append(
                            Some(turn),
                            EventPayload::ModelResponseRecorded {
                                text: crate::runtime::ModelText(text.clone()),
                            },
                        )
                        .await
                    {
                        let _ = self
                            .append(
                                Some(turn),
                                EventPayload::TurnFailed {
                                    error: error.to_string(),
                                },
                            )
                            .await;
                        return Err(error);
                    }
                    if let Err(error) = self
                        .append(
                            Some(turn),
                            EventPayload::TurnCompleted {
                                text: crate::runtime::ModelText(text.clone()),
                            },
                        )
                        .await
                    {
                        let _ = self
                            .append(
                                Some(turn),
                                EventPayload::TurnFailed {
                                    error: error.to_string(),
                                },
                            )
                            .await;
                        return Err(error);
                    }
                    tracing::info!(target: "mini_harness::turn", turn_id = %turn, final_len = text.len(), "turn completed");
                    return Ok((turn, text));
                }
                ModelResponse::ToolCall(call) => (vec![call], None),
                ModelResponse::ToolCalls(calls) => (calls, None),
                ModelResponse::TextWithToolCalls { text, calls } => (calls, Some(text)),
            };
            if calls.is_empty() {
                let error = ProviderError::from("provider returned an empty tool-call batch");
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: error.to_string(),
                    },
                )
                .await?;
                return Err(HarnessError::Provider(error));
            }
            let mut call_ids = HashSet::with_capacity(calls.len());
            if calls.iter().any(|call| !call_ids.insert(call.call_id)) {
                let error = ProviderError::from("provider returned duplicate tool call id");
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: error.to_string(),
                    },
                )
                .await?;
                return Err(HarnessError::Provider(error));
            }
            if calls.len() > self.config.max_batch_tool_calls {
                // Fail before appending any per-call events so an oversized
                // batch cannot grow per-turn state (executions, inputs)
                // without bound.
                let error = ProviderError::from(format!(
                    "tool call batch of {} exceeds the {} call limit",
                    calls.len(),
                    self.config.max_batch_tool_calls
                ));
                self.append(
                    Some(turn),
                    EventPayload::TurnFailed {
                        error: error.to_string(),
                    },
                )
                .await?;
                return Err(HarnessError::Provider(error));
            }
            let call_names = calls
                .iter()
                .map(|call| call.name.0.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let response_text = response_text.unwrap_or_else(|| {
                if calls.len() == 1 {
                    format!("tool call: {call_names}")
                } else {
                    format!("tool calls: {call_names}")
                }
            });
            if let Err(error) = self
                .append(
                    Some(turn),
                    EventPayload::ModelResponseRecorded {
                        text: crate::runtime::ModelText(response_text),
                    },
                )
                .await
            {
                let _ = self
                    .append(
                        Some(turn),
                        EventPayload::TurnFailed {
                            error: error.to_string(),
                        },
                    )
                    .await;
                return Err(error);
            }
            let deferred_calls = calls.clone();
            for (call_index, call) in calls.into_iter().enumerate() {
                let call_id = call.call_id;
                let call_name = call.name.clone();
                let call_input = call.input.clone();
                if let Err(error) = self
                    .append(
                        Some(turn),
                        EventPayload::ToolRequested {
                            call_id,
                            name: call_name.clone(),
                            input: call.input.clone(),
                        },
                    )
                    .await
                {
                    let _ = self
                        .append(
                            Some(turn),
                            EventPayload::TurnFailed {
                                error: error.to_string(),
                            },
                        )
                        .await;
                    return Err(error);
                }

                let tool = match self.tools.lookup(&call_name.0) {
                    Ok(tool) => tool,
                    Err(error) => {
                        let message = error.to_string();
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
                };
                let workspace = self.executor.workspace_root();
                match self
                    .policy
                    .decide(&tool.spec(), &call.input, workspace.as_ref())
                {
                    PolicyDecision::Allow => {}
                    PolicyDecision::Ask => {
                        self.append(Some(turn), EventPayload::ToolApprovalRequested { call_id })
                            .await?;
                        let deferred = deferred_calls
                            .iter()
                            .skip(call_index.saturating_add(1))
                            .map(|candidate| candidate.call_id)
                            .collect::<Vec<_>>();
                        if !deferred.is_empty() {
                            self.append(
                                Some(turn),
                                EventPayload::ToolBatchDeferred {
                                    call_ids: deferred,
                                    reason: "tool batch paused for approval".into(),
                                },
                            )
                            .await?;
                        }
                        return Err(HarnessError::ApprovalPending(
                            format!("approval required for tool `{}`", call_name.0).into(),
                        ));
                    }
                    PolicyDecision::Deny => {
                        let reason = format!("policy denied tool `{}`", call_name.0);
                        // Record the harness decision explicitly instead of
                        // faking a user approval response: the durable log
                        // must be able to answer "was the user ever asked?".
                        self.append(
                            Some(turn),
                            EventPayload::ToolPolicyDenied {
                                call_id,
                                reason: reason.clone(),
                            },
                        )
                        .await?;
                        self.append(
                            Some(turn),
                            EventPayload::TurnFailed {
                                error: reason.clone(),
                            },
                        )
                        .await?;
                        return Err(HarnessError::Policy(reason.into()));
                    }
                }

                let execution_id = crate::runtime::ExecutionId::new();
                self.append(
                    Some(turn),
                    EventPayload::ToolStarted {
                        call_id,
                        execution_id,
                    },
                )
                .await?;
                tracing::info!(
                    target: "mini_harness::tool",
                    turn_id = %turn,
                    call_id = %call_id,
                    execution_id = %execution_id,
                    tool = %call_name.0,
                    "tool execution started"
                );
                let executor = Arc::clone(&self.executor);
                let tool_cancel = cancel.clone();
                let output_limit = self
                    .config
                    .max_tool_result_bytes
                    .min(DEFAULT_TOOL_OUTPUT_LIMIT);
                let tool_started_at = Instant::now();
                let tool_deadline = bounded_tool_deadline(
                    tool_started_at,
                    turn_deadline,
                    self.tool_time_used,
                    self.config.max_tool_time,
                );
                let mut task = tokio::spawn(async move {
                    tool.execute(
                        call_input,
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
                        if matches!(cancellation_reason, CancellationReason::Cancelled) {
                            self.append(Some(turn), EventPayload::TurnCancelRequested)
                                .await?;
                        }
                        let outcome_unknown = match tokio::time::timeout(
                            TOOL_CLEANUP_TIMEOUT,
                            &mut task,
                        )
                        .await
                        {
                            Ok(Ok(Ok(value))) => {
                                if value.len() > self.config.max_tool_result_bytes {
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
                                } else if let Err(error) = self
                                    .append(
                                        Some(turn),
                                        EventPayload::ToolCompleted {
                                            call_id,
                                            result: crate::runtime::ToolResult(value),
                                        },
                                    )
                                    .await
                                {
                                    self.append(
                                        Some(turn),
                                        EventPayload::ToolFailed {
                                            call_id,
                                            error: error.to_string().into(),
                                        },
                                    )
                                    .await?;
                                }
                                false
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
                                false
                            }
                            Ok(Err(join_error)) => {
                                self.append(
                                    Some(turn),
                                    EventPayload::ToolOutcomeUnknown {
                                        call_id,
                                        reason: tool_join_error(join_error),
                                    },
                                )
                                .await?;
                                true
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
                                true
                            }
                        };
                        self.record_tool_time(tool_started_at);
                        if !outcome_unknown {
                            self.append(Some(turn), cancellation_reason.event()).await?;
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
                                    "cumulative tool time limit exceeded during tool execution"
                                        .into()
                                } else {
                                    "turn wall-clock limit exceeded during tool execution".into()
                                },
                            },
                        )
                        .await?;
                        return Err(HarnessError::Timeout);
                    }
                    result = &mut task => match result {
                        Ok(result) => result,
                        Err(join_error) => Err(tool_join_error(join_error)),
                    }
                };
                self.record_tool_time(tool_started_at);
                match result {
                    Ok(value) => {
                        if value.len() > self.config.max_tool_result_bytes {
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
                        let result_len = value.len();
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
                        tracing::info!(
                            target: "mini_harness::tool",
                            turn_id = %turn,
                            call_id = %call_id,
                            result_len,
                            "tool execution completed"
                        );
                        if cancel.is_cancelled() {
                            self.append_cancellation(turn, cancellation_reason).await?;
                            return Err(HarnessError::Cancelled);
                        }
                    }
                    Err(err) => {
                        self.append(
                            Some(turn),
                            EventPayload::ToolFailed {
                                call_id,
                                error: err.clone().into(),
                            },
                        )
                        .await?;
                        tracing::warn!(
                            target: "mini_harness::tool",
                            turn_id = %turn,
                            call_id = %call_id,
                            error = %err,
                            "tool execution failed"
                        );
                        if cancel.is_cancelled() {
                            self.append_cancellation(turn, cancellation_reason).await?;
                            return Err(HarnessError::Cancelled);
                        }
                        self.append(Some(turn), EventPayload::TurnFailed { error: err.clone() })
                            .await?;
                        return Err(HarnessError::Tool(err.into()));
                    }
                }
            }
        }
    }
}

pub(super) fn tool_join_error(error: JoinError) -> String {
    if error.is_panic() {
        "tool task panicked".into()
    } else {
        "tool task was cancelled".into()
    }
}
