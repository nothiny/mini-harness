use super::{
    agent_loop::{AgentLoop, AgentLoopConfig},
    ids::{SessionId, TurnId},
    state::SessionState,
};
use crate::{
    durable::EventStore,
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    policy::{DefaultPolicy, ToolPolicy},
    tools::ToolRegistry,
};
use std::sync::Arc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

/// A single-task, sequentially driven session facade over [`AgentLoop`].
///
/// 简化版只有一个同步的 `Session` 接口：turn 结束前不能开始新 turn
/// （返回 [`HarnessError::TurnAlreadyActive`]），需要并发控制（例如
/// 协议服务器在 turn 运行时响应取消/审批）时，在外层自行组合
/// `tokio::spawn` + channel —— 见 `mini-harness-server`。
///
/// While `start_turn` is awaited, `cancel_turn`/`approve_tool` cannot be
/// called on the same value (the borrow checker forbids it). Mid-turn
/// control is available through:
///
/// * [`Session::start_turn_with_timeout`] for a wall-clock deadline;
/// * [`Session::cancel_turn`] / [`Session::approve_tool`] once a turn is
///   paused with [`HarnessError::ApprovalPending`];
/// * `mini-harness cancel <session-id> <turn-id>` across processes via the
///   shared event log.
pub struct Session<P, E, S> {
    loop_: AgentLoop<P, E, S>,
}

impl<P, E, S> Session<P, E, S>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    pub fn new(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
    ) -> Self {
        Self {
            loop_: AgentLoop::new(SessionId::new(), provider, executor, tools, store),
        }
    }

    pub fn new_with_policy(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        policy: Arc<dyn ToolPolicy>,
    ) -> Self {
        Self {
            loop_: AgentLoop::new_with_policy(
                SessionId::new(),
                provider,
                executor,
                tools,
                store,
                policy,
            ),
        }
    }

    /// Creates a session with a caller-chosen id and resource limits. Used
    /// when the durable layout derives the event-log path from the id.
    pub fn new_with_policy_and_config(
        session_id: SessionId,
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Self {
        Self {
            loop_: AgentLoop::new_with_config(
                session_id, provider, executor, tools, store, policy, config,
            ),
        }
    }

    /// Resumes a session by replaying its full event log.
    pub async fn resume(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        session_id: SessionId,
    ) -> Result<Self, HarnessError> {
        Self::resume_with_policy(
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

    pub async fn resume_with_policy(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        session_id: SessionId,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Result<Self, HarnessError> {
        Ok(Self {
            loop_: AgentLoop::restore(provider, executor, tools, store, session_id, policy, config)
                .await?,
        })
    }

    /// Runs one turn to completion. See the type-level docs: while this
    /// future is pending, `cancel_turn`/`approve_tool` cannot be called on
    /// the same `Session` value.
    pub async fn start_turn(&mut self, input: String) -> Result<(TurnId, String), HarnessError> {
        self.loop_.run_turn(input).await
    }

    /// 记录输入并启动 turn，立即返回 turn id（不执行采样）。
    /// 与 [`Self::continue_turn`] 组合使用：协议服务器先用
    /// `begin_turn` 拿到 id 回复客户端，再异步驱动执行。
    pub async fn begin_turn(&mut self, input: String) -> Result<TurnId, HarnessError> {
        let turn = TurnId::new();
        self.loop_.begin_turn(turn, input).await?;
        Ok(turn)
    }

    /// 继续驱动一个已 begin 的 turn 直到终止。
    pub async fn continue_turn(
        &mut self,
        turn: TurnId,
        cancel: CancellationToken,
    ) -> Result<(TurnId, String), HarnessError> {
        self.loop_.continue_turn(turn, cancel).await
    }

    pub async fn start_turn_with_timeout(
        &mut self,
        input: String,
        timeout: Duration,
    ) -> Result<(TurnId, String), HarnessError> {
        self.loop_.run_turn_with_timeout(input, timeout).await
    }

    pub fn set_system_instructions(&mut self, instructions: impl Into<String>) {
        self.loop_.set_system_instructions(instructions);
    }

    pub fn state(&self) -> &SessionState {
        &self.loop_.state
    }

    /// Requests cancellation of an active turn.
    ///
    /// This is only reachable once no turn is running on this `Session`
    /// (for example after an `ApprovalPending` error) or from a separate
    /// process through the CLI. For concurrent cancellation use a turn
    /// timeout or an outer task that cancels the turn's future.
    pub async fn cancel_turn(&mut self, turn: TurnId) -> Result<(), HarnessError> {
        self.loop_.cancel_turn(turn).await
    }

    /// 对挂起的审批做出决定并恢复 turn。
    pub async fn approve_tool(
        &mut self,
        turn: TurnId,
        call_id: super::ids::ToolCallId,
        approved: bool,
    ) -> Result<(TurnId, String), HarnessError> {
        self.loop_
            .approve_tool(turn, call_id, approved, CancellationToken::new())
            .await
    }
}
