use crate::{
    durable::{Event, EventPayload, EventStore, ToolOutcome, reduce},
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
            max_context_bytes: 1024 * 1024,
            max_event_bytes: 1024 * 1024,
            max_tool_result_bytes: DEFAULT_TOOL_OUTPUT_LIMIT,
        }
    }
}

pub(super) const DEFAULT_MAX_BATCH_TOOL_CALLS: usize = 16;

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

/// Agent Loop: 模型-工具循环的核心实现。
///
/// # 执行流程
///
/// ```text
/// ┌─────────────────────────────────────┐
/// │  1. Sample 模型                      │
/// │     ↓                               │
/// │  2. 解析响应                         │
/// │     ├─→ 文本回答 → 结束 Turn        │
/// │     └─→ 工具调用 → 继续             │
/// │           ↓                         │
/// │  3. 执行工具（带审批）               │
/// │     ↓                               │
/// │  4. 追加结果到 context              │
/// │     ↓                               │
/// │  5. 返回步骤 1（直到达到 max_steps） │
/// └─────────────────────────────────────┘
/// ```
///
/// # 为什么需要步数限制？
///
/// 防止模型陷入无限循环（例如反复调用同一工具）。
///
/// # 为什么需要工具审批？
///
/// 防止模型执行危险操作（例如 `bash rm -rf /`）。
///
/// # 事件协议
///
/// 每次工具调用恰好追加两类事件：
/// `tool.requested`（含 `requires_approval`）→ 执行 → `tool.completed`
/// （成功或失败合并记录）。`ask` 工具在执行前额外获得一个
/// `tool.approval.responded` 事件，并且 turn 以 [`HarnessError::ApprovalPending`]
/// 暂停，直到用户通过 [`AgentLoop::approve_tool`] 恢复。
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
    /// 追加一个事件: 校验 → 持久化 → 提交状态转换。
    ///
    /// 顺序是关键：先确认 store 与内存状态同步（另一个进程可能已经
    /// 写入了终止事件，例如跨进程 cancel），再把事件交给 reducer
    /// 预演，全部通过后才真正落盘并更新内存状态。
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
        // 注意: 这里不再检查累积 history 是否超过 `max_context_bytes`。
        // 那个限制属于滑动窗口后的模型请求（见 continue_turn）；累积
        // 审计历史的上限由 reducer 的 `MAX_HISTORY_BYTES` 独立保护，
        // 两者共用一个限制会误拒长会话（ARCHITECTURE_REVIEW P0-2）。
        reduce(&mut next_state, &event)?;
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
    /// 运行一个完整 turn: 记录输入 → 循环采样直到模型给出最终文本。
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

    /// 记录用户输入并启动 turn。
    ///
    /// 简化版没有 durable queue：一次只允许一个 turn，运行中（或还有
    /// 未消费输入时）到达的新输入以 [`HarnessError::TurnAlreadyActive`]
    /// 直接拒绝。
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
        self.append(Some(turn), EventPayload::TurnStarted).await?;
        self.turn_started_at = Some(Instant::now());
        self.turn_steps = 0;
        self.tool_time_used = Duration::ZERO;
        tracing::info!(target: "mini_harness::turn", turn_id = %turn, "turn started");
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
                // batch cannot grow per-turn state without bound.
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
            for call in calls.into_iter() {
                self.execute_tool_call(turn, call, &cancel, cancellation_reason)
                    .await?;
            }
        }
    }

    /// 执行单个工具调用（含策略检查与审批暂停）。
    ///
    /// 事件序列：
    /// 1. `tool.requested { requires_approval }` —— 总是首先追加；
    ///    `requires_approval: true` 即客户端可见的"审批请求"。
    /// 2. 若策略为 Ask → 返回 [`HarnessError::ApprovalPending`]，turn
    ///    保持活跃；用户决定后由 [`AgentLoop::approve_tool`] 记录
    ///    `tool.approval.responded` 并继续执行。
    /// 3. 执行 → `tool.completed { outcome, duration_ms }`。
    async fn execute_tool_call(
        &mut self,
        turn: TurnId,
        call: crate::model::ToolCall,
        cancel: &CancellationToken,
        cancellation_reason: CancellationReason,
    ) -> Result<(), HarnessError> {
        let call_id = call.call_id;
        let call_name = call.name.clone();
        let call_input = call.input.clone();
        // 先解析工具与校验输入: 失败不终止 turn，把错误作为工具结果
        // 返回给模型，让它有机会在下一步纠正参数。
        let resolved = match self.tools.lookup(&call_name.0) {
            Ok(tool) => {
                if let Err(error) = tool.validate_input(&call_input) {
                    let outcome = ToolOutcome::Rejected {
                        error: error.into(),
                    };
                    self.append(
                        Some(turn),
                        EventPayload::ToolRequested {
                            call_id,
                            name: call_name.clone(),
                            input: call_input.clone(),
                            requires_approval: false,
                        },
                    )
                    .await?;
                    return self
                        .append_tool_completed(turn, call_id, &outcome, Duration::ZERO)
                        .await;
                }
                tool
            }
            Err(error) => {
                let outcome = ToolOutcome::Rejected {
                    error: error.to_string().into(),
                };
                self.append(
                    Some(turn),
                    EventPayload::ToolRequested {
                        call_id,
                        name: call_name.clone(),
                        input: call_input.clone(),
                        requires_approval: false,
                    },
                )
                .await?;
                return self
                    .append_tool_completed(turn, call_id, &outcome, Duration::ZERO)
                    .await;
            }
        };
        let requires_approval = matches!(
            self.policy.decide(&resolved.spec(), &call_input),
            PolicyDecision::Ask
        );
        if let Err(error) = self
            .append(
                Some(turn),
                EventPayload::ToolRequested {
                    call_id,
                    name: call_name.clone(),
                    input: call_input.clone(),
                    requires_approval,
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
        if requires_approval {
            // turn 在此暂停（不追加任何终止事件）。审批决策本身就是
            // durable 事实，由 approve_tool 记录。
            return Err(HarnessError::ApprovalPending(
                format!("approval required for tool `{}`", call_name.0).into(),
            ));
        }
        self.run_approved_tool(
            turn,
            call_id,
            call_input,
            cancel.clone(),
            cancellation_reason,
        )
        .await
    }

    /// 执行一个已通过审批（或无需审批）的工具调用并记录结果。
    pub(super) async fn run_approved_tool(
        &mut self,
        turn: TurnId,
        call_id: crate::runtime::ToolCallId,
        input: serde_json::Value,
        cancel: CancellationToken,
        cancellation_reason: CancellationReason,
    ) -> Result<(), HarnessError> {
        let turn_started_at = self.turn_started_at.unwrap_or_else(Instant::now);
        let turn_deadline = turn_deadline_for(turn_started_at, self.config.max_turn_duration);
        let tool_started_at = Instant::now();
        let tool_deadline = bounded_tool_deadline(
            tool_started_at,
            turn_deadline,
            self.tool_time_used,
            self.config.max_tool_time,
        );
        let execution_id = crate::runtime::ExecutionId::new();
        let tool_name = self
            .state
            .active_turn
            .as_ref()
            .and_then(|turn| turn.tool_names.get(&call_id))
            .cloned()
            .unwrap_or_else(|| crate::runtime::ToolName(String::new()));
        let tool = match self.tools.lookup(&tool_name.0) {
            Ok(tool) => tool,
            Err(error) => {
                let outcome = ToolOutcome::Rejected {
                    error: error.to_string().into(),
                };
                return self
                    .append_tool_completed(turn, call_id, &outcome, Duration::ZERO)
                    .await;
            }
        };
        let workspace = self.executor.workspace_root();
        tracing::info!(
            target: "mini_harness::tool",
            turn_id = %turn,
            call_id = %call_id,
            execution_id = %execution_id,
            tool = %tool_name.0,
            "tool execution started"
        );
        let executor = Arc::clone(&self.executor);
        let tool_cancel = cancel.clone();
        let output_limit = self
            .config
            .max_tool_result_bytes
            .min(DEFAULT_TOOL_OUTPUT_LIMIT);
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
                            let outcome = ToolOutcome::Failed {
                                error: format!(
                                    "tool result exceeds the {} byte limit",
                                    self.config.max_tool_result_bytes
                                )
                                .into(),
                            };
                            self.append_tool_completed(
                                turn,
                                call_id,
                                &outcome,
                                tool_started_at.elapsed(),
                            )
                            .await?;
                        } else {
                            let outcome = ToolOutcome::Succeeded {
                                result: crate::runtime::ToolResult(value),
                            };
                            self.append_tool_completed(
                                turn,
                                call_id,
                                &outcome,
                                tool_started_at.elapsed(),
                            )
                            .await?;
                        }
                        false
                    }
                    Ok(Ok(Err(error))) => {
                        let outcome = ToolOutcome::Failed {
                            error: error.into(),
                        };
                        self.append_tool_completed(
                            turn,
                            call_id,
                            &outcome,
                            tool_started_at.elapsed(),
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
                            "cumulative tool time limit exceeded during tool execution".into()
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
                    let outcome = ToolOutcome::Failed {
                        error: message.clone().into(),
                    };
                    self.append_tool_completed(turn, call_id, &outcome, tool_started_at.elapsed())
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
                let outcome = ToolOutcome::Succeeded {
                    result: crate::runtime::ToolResult(value),
                };
                if let Err(error) = self
                    .append_tool_completed(turn, call_id, &outcome, tool_started_at.elapsed())
                    .await
                {
                    let message = error.to_string();
                    let _ = self
                        .append(Some(turn), EventPayload::TurnFailed { error: message })
                        .await;
                    return Err(error);
                }
                tracing::info!(
                    target: "mini_harness::tool",
                    turn_id = %turn,
                    call_id = %call_id,
                    "tool execution completed"
                );
                if cancel.is_cancelled() {
                    self.append_cancellation(turn, cancellation_reason).await?;
                    return Err(HarnessError::Cancelled);
                }
                Ok(())
            }
            Err(err) => {
                let outcome = ToolOutcome::Failed {
                    error: err.clone().into(),
                };
                self.append_tool_completed(turn, call_id, &outcome, tool_started_at.elapsed())
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
                Err(HarnessError::Tool(err.into()))
            }
        }
    }

    /// 追加 `tool.completed` 事件；失败时退化为 `tool.outcome_unknown`。
    async fn append_tool_completed(
        &mut self,
        turn: TurnId,
        call_id: crate::runtime::ToolCallId,
        outcome: &ToolOutcome,
        duration: Duration,
    ) -> Result<(), HarnessError> {
        let duration_ms = duration.as_millis().min(u64::MAX as u128) as u64;
        self.append(
            Some(turn),
            EventPayload::ToolCompleted {
                call_id,
                outcome: outcome.clone(),
                duration_ms,
            },
        )
        .await
    }
}

pub(super) fn tool_join_error(error: JoinError) -> String {
    if error.is_panic() {
        "tool task panicked".into()
    } else {
        "tool task was cancelled".into()
    }
}
