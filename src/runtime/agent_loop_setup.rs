use super::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::{
    durable::{EventStore, reduce},
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    policy::{DefaultPolicy, ToolPolicy},
    runtime::{EventSeq, SessionId, SessionState},
    tools::ToolRegistry,
};
use std::sync::Arc;
use tokio::time::Instant as TokioInstant;

impl<P, E, S> AgentLoop<P, E, S>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    pub fn new(
        session_id: SessionId,
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
    ) -> Self {
        Self::new_with_config(
            session_id,
            provider,
            executor,
            tools,
            store,
            Arc::new(DefaultPolicy),
            AgentLoopConfig::default(),
        )
    }

    pub fn new_with_policy(
        session_id: SessionId,
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        policy: Arc<dyn ToolPolicy>,
    ) -> Self {
        Self::new_with_config(
            session_id,
            provider,
            executor,
            tools,
            store,
            policy,
            AgentLoopConfig::default(),
        )
    }

    pub fn new_with_config(
        session_id: SessionId,
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Self {
        Self {
            provider,
            executor,
            tools,
            store,
            state: SessionState::new(session_id),
            system_instructions: None,
            policy,
            config,
            turn_started_at: None,
            turn_steps: 0,
            tool_time_used: std::time::Duration::ZERO,
        }
    }

    pub fn from_state(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        state: SessionState,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Self {
        // A resumed turn gets a fresh wall-clock budget: `turn_started_at` is
        // "now", not the pre-crash start time. This is deliberate (process
        // downtime should not instantly expire a turn) but it means the turn
        // deadline is measured from the restart, not from the original input.
        // `turn_steps` and `tool_time_used` are also reset, so only durable
        // facts, not budgets, survive a restart.
        let turn_started_at = state.active_turn.as_ref().map(|_| TokioInstant::now());
        Self {
            provider,
            executor,
            tools,
            store,
            state,
            system_instructions: None,
            policy,
            config,
            turn_started_at,
            turn_steps: 0,
            tool_time_used: std::time::Duration::ZERO,
        }
    }

    /// 崩溃恢复 = 从事件日志完整重放（简化版没有 checkpoint）。
    ///
    /// 每个事件按序号交给 reducer；任何非法转换都会让恢复失败 ——
    /// 宁可失败也不猜测历史。如果日志末尾有一个仍在运行的 turn
    /// （崩溃前未写入终止事件），它保持 active 状态：调用方可以
    /// `cancel_turn` 放弃它，或对挂起的审批做出决定。
    pub async fn restore(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        session_id: SessionId,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Result<Self, HarnessError> {
        let events = store.read_from(EventSeq(1)).await?;
        if events.is_empty() {
            return Err(HarnessError::InvariantViolation(
                "cannot restore an empty event store".into(),
            ));
        }
        if events[0].session_id != session_id {
            return Err(HarnessError::InvariantViolation(format!(
                "event store belongs to session {}, requested {}",
                events[0].session_id, session_id
            )));
        }
        let mut state = SessionState::new(session_id);
        for event in &events {
            reduce(&mut state, event)?;
        }
        let last_seq = store.last_seq().await?;
        if state.last_seq != last_seq {
            return Err(HarnessError::InvariantViolation(
                "event store changed while restoring session".into(),
            ));
        }
        Ok(Self::from_state(
            provider, executor, tools, store, state, policy, config,
        ))
    }

    pub async fn restore_default(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        session_id: SessionId,
    ) -> Result<Self, HarnessError> {
        Self::restore(
            provider,
            executor,
            tools,
            store,
            session_id,
            Arc::new(DefaultPolicy),
            AgentLoopConfig::default(),
        )
        .await
    }

    pub fn set_system_instructions(&mut self, instructions: impl Into<String>) {
        self.system_instructions = Some(instructions.into());
    }
}
