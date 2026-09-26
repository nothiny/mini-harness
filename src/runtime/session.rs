use super::session_actor;
use super::{
    agent_loop::{AgentLoop, AgentLoopConfig},
    ids::{SessionId, TurnId},
    types::UserInput,
};
use crate::{
    durable::EventStore,
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    policy::{DefaultPolicy, ToolPolicy},
    tools::ToolRegistry,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub type TurnOutcome = Result<String, Arc<HarnessError>>;
pub(super) type Outcome = TurnOutcome;
pub(super) type OutcomeReply = oneshot::Sender<TurnOutcome>;
/// Input submitted while a turn runs, awaiting durable recording and ack.
pub(super) type PendingStart = (
    TurnId,
    String,
    oneshot::Sender<Result<(TurnId, bool), HarnessError>>,
);

pub enum SessionCommand {
    /// Starts a turn, or durably queues the input when a turn is running.
    /// The bool in the reply is `queued`: true when another turn was active
    /// and this input will start after it (design §12 single-session queue).
    StartTurn {
        input: UserInput,
        reply: oneshot::Sender<Result<(TurnId, bool), HarnessError>>,
    },
    CancelTurn {
        turn_id: TurnId,
        reply: oneshot::Sender<Result<(), HarnessError>>,
    },
    ApproveTool {
        turn_id: TurnId,
        call_id: super::ids::ToolCallId,
        approved: bool,
        reply: oneshot::Sender<Result<(TurnId, String), HarnessError>>,
    },
    WaitTurn {
        turn_id: TurnId,
        reply: OutcomeReply,
    },
    /// Appends the checkpoint marker and writes the snapshot. Sent by the
    /// protocol server's event pump at turn boundaries so checkpoint writes
    /// stay serialized with the session's single writer.
    Checkpoint {
        path: PathBuf,
        reply: oneshot::Sender<Result<crate::durable::Checkpoint, HarnessError>>,
    },
    /// Reports the session's scheduler lifecycle state (design §12).
    QueryState {
        reply: oneshot::Sender<super::state::SessionRunState>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), HarnessError>>,
    },
}

#[derive(Clone)]
pub struct SessionHandle {
    sender: mpsc::Sender<SessionCommand>,
}

impl SessionHandle {
    pub async fn start_turn(&self, input: UserInput) -> Result<TurnId, HarnessError> {
        self.start_turn_with_status(input)
            .await
            .map(|(turn, _)| turn)
    }

    /// Like [`Self::start_turn`] but also reports whether the input was
    /// queued behind a running turn instead of starting immediately.
    pub async fn start_turn_with_status(
        &self,
        input: UserInput,
    ) -> Result<(TurnId, bool), HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::StartTurn { input, reply })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?
    }

    pub async fn cancel_turn(&self, turn_id: TurnId) -> Result<(), HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::CancelTurn { turn_id, reply })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?
    }

    pub async fn approve_tool(
        &self,
        turn_id: TurnId,
        call_id: super::ids::ToolCallId,
        approved: bool,
    ) -> Result<(TurnId, String), HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::ApproveTool {
                turn_id,
                call_id,
                approved,
                reply,
            })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?
    }

    /// Applies an approval decision while preserving command order.
    ///
    /// The returned result is sent only after the actor has persisted the
    /// approval event and completed the resumed turn (or recorded its
    /// terminal error). This gives protocol callers a durable acknowledgement
    /// instead of an enqueue-only success.
    pub async fn queue_approval(
        &self,
        turn_id: TurnId,
        call_id: super::ids::ToolCallId,
        approved: bool,
    ) -> Result<(TurnId, String), HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::ApproveTool {
                turn_id,
                call_id,
                approved,
                reply,
            })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?
    }

    pub async fn wait_turn(&self, turn_id: TurnId) -> TurnOutcome {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::WaitTurn { turn_id, reply })
            .await
            .map_err(|_| {
                Arc::new(HarnessError::InvariantViolation(
                    "session actor stopped".into(),
                ))
            })?;
        result.await.map_err(|_| {
            Arc::new(HarnessError::InvariantViolation(
                "session actor stopped".into(),
            ))
        })?
    }

    /// Asks the actor to persist a checkpoint at a safe point. The command is
    /// serialized with turns: if a turn is running it completes (or reaches a
    /// terminal state) before the snapshot is taken.
    pub async fn create_checkpoint(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<crate::durable::Checkpoint, HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::Checkpoint {
                path: path.as_ref().to_path_buf(),
                reply,
            })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?
    }

    /// Reports the session's scheduler lifecycle state.
    pub async fn query_state(&self) -> Result<super::state::SessionRunState, HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::QueryState { reply })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))
    }

    pub async fn shutdown(&self) {
        let _ = self.shutdown_with_result().await;
    }

    pub async fn shutdown_with_result(&self) -> Result<(), HarnessError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(SessionCommand::Shutdown { reply })
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?;
        result
            .await
            .map_err(|_| HarnessError::InvariantViolation("session actor stopped".into()))?
    }
}

pub fn spawn<P, E, S>(
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    spawn_with_id(SessionId::new(), provider, executor, tools, store)
}

pub fn spawn_with_id<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let loop_ = AgentLoop::new(session_id, provider, executor, tools, store);
    spawn_actor(loop_)
}

pub fn spawn_with_policy<P, E, S>(
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    policy: Arc<dyn ToolPolicy>,
) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    spawn_with_policy_id(SessionId::new(), provider, executor, tools, store, policy)
}

pub fn spawn_with_policy_id<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    policy: Arc<dyn ToolPolicy>,
) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let loop_ = AgentLoop::new_with_policy(session_id, provider, executor, tools, store, policy);
    spawn_actor(loop_)
}

/// Spawns a brand-new session actor (no prior events) with explicit limits.
pub fn spawn_session_with_config<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    policy: Arc<dyn ToolPolicy>,
    config: AgentLoopConfig,
) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let loop_ =
        AgentLoop::new_with_config(session_id, provider, executor, tools, store, policy, config);
    spawn_actor(loop_)
}

pub async fn resume_with_policy_id<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    policy: Arc<dyn ToolPolicy>,
) -> Result<(SessionHandle, JoinHandle<()>), HarnessError>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    resume_with_policy_id_and_config(
        session_id,
        provider,
        executor,
        tools,
        store,
        policy,
        AgentLoopConfig::default(),
    )
    .await
}

/// Resumes (or adopts a freshly created) session actor with explicit resource
/// limits; used by the protocol server whose limits come from configuration.
pub async fn resume_with_policy_id_and_config<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    policy: Arc<dyn ToolPolicy>,
    config: AgentLoopConfig,
) -> Result<(SessionHandle, JoinHandle<()>), HarnessError>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let loop_ =
        AgentLoop::restore(provider, executor, tools, store, session_id, policy, config).await?;
    Ok(spawn_actor(loop_))
}

/// Resumes an actor-backed session from a checkpoint when possible.  The
/// event log remains authoritative and is replayed in full if the snapshot is
/// unavailable or fails validation.
pub async fn resume_with_policy_id_and_checkpoint<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    checkpoint_path: impl AsRef<std::path::Path>,
    policy: Arc<dyn ToolPolicy>,
) -> Result<(SessionHandle, JoinHandle<()>), HarnessError>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    resume_with_policy_id_checkpoint_and_config(
        session_id,
        provider,
        executor,
        tools,
        store,
        checkpoint_path,
        policy,
        AgentLoopConfig::default(),
    )
    .await
}

/// Checkpoint-resume with explicit resource limits (configuration-driven).
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_policy_id_checkpoint_and_config<P, E, S>(
    session_id: SessionId,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    checkpoint_path: impl AsRef<std::path::Path>,
    policy: Arc<dyn ToolPolicy>,
    config: AgentLoopConfig,
) -> Result<(SessionHandle, JoinHandle<()>), HarnessError>
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let loop_ = AgentLoop::restore_with_checkpoint(
        provider,
        executor,
        tools,
        store,
        session_id,
        checkpoint_path,
        policy,
        config,
    )
    .await?;
    Ok(spawn_actor(loop_))
}

pub fn spawn_with_system_instructions<P, E, S>(
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    instructions: impl Into<String>,
) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let mut loop_ = AgentLoop::new(SessionId::new(), provider, executor, tools, store);
    loop_.set_system_instructions(instructions);
    spawn_actor(loop_)
}

fn spawn_actor<P, E, S>(loop_: AgentLoop<P, E, S>) -> (SessionHandle, JoinHandle<()>)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let (sender, receiver) = mpsc::channel(16);
    let handle = SessionHandle { sender };
    (
        handle,
        tokio::spawn(session_actor::run_actor(receiver, loop_)),
    )
}

/// A single-task, sequentially driven session facade over [`AgentLoop`].
///
/// This type owns the loop mutably, so a turn can only be cancelled from the
/// outside while `start_turn` is awaited on another task — which the borrow
/// checker forbids. Use one of these instead when you need mid-turn control:
///
/// * [`session::spawn`](super::session::spawn) (the session actor) accepts
///   `CancelTurn`/`ApproveTool`/`WaitTurn` commands while a turn runs.
/// * [`Session::start_turn_with_timeout`] enforces a wall-clock deadline.
/// * `mini-harness cancel <session-id> <turn-id>` cancels across processes by
///   appending durable events to the shared event log.
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

    /// Resumes a session using a checkpoint when available, falling back to a
    /// full event-log replay when the snapshot cannot be used.
    #[allow(clippy::too_many_arguments)]
    pub async fn resume_with_policy_and_checkpoint(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        session_id: SessionId,
        checkpoint_path: impl AsRef<Path>,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Result<Self, HarnessError> {
        Ok(Self {
            loop_: AgentLoop::restore_with_checkpoint(
                provider,
                executor,
                tools,
                store,
                session_id,
                checkpoint_path,
                policy,
                config,
            )
            .await?,
        })
    }

    /// Runs one turn to completion. See the type-level docs: while this
    /// future is pending, `cancel_turn`/`approve_tool` cannot be called on
    /// the same `Session` value.
    pub async fn start_turn(&mut self, input: String) -> Result<(TurnId, String), HarnessError> {
        self.loop_.run_turn(input).await
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

    pub fn state(&self) -> &super::state::SessionState {
        &self.loop_.state
    }

    /// Writes a checkpoint for the current state and advances the in-memory
    /// reducer through the corresponding durable marker event.
    pub async fn create_checkpoint(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<crate::durable::Checkpoint, HarnessError> {
        self.loop_.create_checkpoint(path).await
    }

    /// Requests cancellation of an active turn.
    ///
    /// This is only reachable once no turn is running on this `Session`
    /// (for example after an `ApprovalPending` error) or from a separate
    /// process through the CLI. For concurrent cancellation use the session
    /// actor or a turn timeout.
    pub async fn cancel_turn(&mut self, turn: TurnId) -> Result<(), HarnessError> {
        self.loop_.cancel_turn(turn).await
    }

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
