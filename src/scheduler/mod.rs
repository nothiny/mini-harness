//! Session scheduling facade (design §12, plan §14 option B).
//!
//! The scheduler owns no runtime state of its own. Per-session serial
//! execution and the durable input queue live in the session actor; the
//! global executor concurrency limit lives in [`LocalExecutor`]. This module
//! composes them into one multi-session API:
//!
//! * `submit` never drops an input silently — it is either durably recorded
//!   (immediately or behind the running turn) or explicitly rejected with
//!   [`HarnessError::QueueLimitExceeded`].
//! * `run_state` projects each session onto the lifecycle
//!   `Idle → RunningTurn → WaitingApproval → RunningTool → RunningTurn → Idle`.
//!
//! Not implemented on purpose (documented in `docs/implementation-plan.md`):
//! priorities, background tasks, and signal merging; cancelling a queued
//! input before it starts (let it start, then cancel — or abandon the turn),
//! because removing it from the durable queue would itself need a
//! "dropped with reason" fact that nothing consumes yet.

mod task;

pub use task::project_run_state as run_state;
pub use task::{SchedulerTask, SessionRunState};

use crate::{
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    runtime::{
        UserInput,
        ids::{SessionId, TurnId},
        session::{SessionHandle, TurnOutcome},
    },
    tools::ToolRegistry,
};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Inputs accepted per session while a turn is running.
    pub max_queued_inputs: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_queued_inputs: 16,
        }
    }
}

/// Multi-session scheduler over session actors.
pub struct Scheduler {
    config: SchedulerConfig,
    handles: tokio::sync::Mutex<HashMap<SessionId, SessionHandle>>,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            handles: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Registers a session actor created by the caller. The scheduler never
    /// constructs providers or stores itself; it schedules accepted inputs.
    pub async fn register_session(&self, session_id: SessionId, handle: SessionHandle) {
        self.handles.lock().await.insert(session_id, handle);
    }

    /// Removes a session after shutting it down.
    pub async fn shutdown_session(&self, session_id: SessionId) -> Result<(), HarnessError> {
        let handle = self.handles.lock().await.remove(&session_id);
        match handle {
            Some(handle) => handle.shutdown_with_result().await,
            None => Err(HarnessError::InvariantViolation("unknown session".into())),
        }
    }

    async fn handle(&self, session_id: SessionId) -> Result<SessionHandle, HarnessError> {
        self.handles
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| HarnessError::InvariantViolation("unknown session".into()))
    }

    /// Submits an input. Starts immediately when the session is idle, or is
    /// durably queued behind the running turn otherwise.
    pub async fn submit(
        &self,
        session_id: SessionId,
        input: impl Into<String>,
    ) -> Result<SchedulerTask, HarnessError> {
        let handle = self.handle(session_id).await?;
        let (turn, _queued) = handle
            .start_turn_with_status(UserInput(input.into()))
            .await?;
        Ok(SchedulerTask::new(session_id, turn))
    }

    /// Waits for a submitted task to reach a terminal state.
    pub async fn wait(&self, task: SchedulerTask) -> TurnOutcome {
        match self.handle(task.session).await {
            Ok(handle) => handle.wait_turn(task.turn).await,
            Err(error) => Err(Arc::new(error)),
        }
    }

    /// Cancels the active turn of a session. Cancelling a still-queued input
    /// is intentionally unsupported (see the module docs).
    pub async fn cancel(&self, session_id: SessionId, turn: TurnId) -> Result<(), HarnessError> {
        self.handle(session_id).await?.cancel_turn(turn).await
    }

    /// Approves or denies a pending tool call.
    pub async fn respond_approval(
        &self,
        session_id: SessionId,
        turn: TurnId,
        call: crate::runtime::ToolCallId,
        approved: bool,
    ) -> Result<(TurnId, String), HarnessError> {
        self.handle(session_id)
            .await?
            .approve_tool(turn, call, approved)
            .await
    }

    /// Projects a session onto the scheduler lifecycle.
    pub async fn run_state(&self, session_id: SessionId) -> Result<SessionRunState, HarnessError> {
        self.handle(session_id).await?.query_state().await
    }
}

/// Spawns a scheduler-managed session actor. Convenience wrapper so callers
/// do not have to repeat the register step.
///
/// The caller keeps the actor's `JoinHandle`; the scheduler only keeps the
/// command handle.
pub async fn spawn_scheduled_session<P, E, S>(
    scheduler: &Scheduler,
    provider: Arc<P>,
    executor: Arc<E>,
    tools: Arc<ToolRegistry>,
    store: Arc<S>,
    policy: Arc<dyn crate::policy::ToolPolicy>,
    loop_config: crate::runtime::agent_loop::AgentLoopConfig,
) -> SessionId
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: crate::durable::EventStore + 'static,
{
    let session_id = SessionId::new();
    let (handle, _actor) = crate::runtime::session::spawn_session_with_config(
        session_id,
        provider,
        executor,
        tools,
        store,
        policy,
        loop_config,
    );
    scheduler.register_session(session_id, handle).await;
    session_id
}
