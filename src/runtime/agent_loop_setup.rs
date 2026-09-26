use super::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::{
    durable::{EventStore, create_checkpoint, reduce, replay_with_checkpoint_fallback},
    error::HarnessError,
    executor::Executor,
    model::ModelProvider,
    policy::{DefaultPolicy, ToolPolicy},
    runtime::{EventSeq, SessionId, SessionState},
    tools::ToolRegistry,
};
use std::{path::Path, sync::Arc};
use tokio::time::Instant;

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
        let turn_started_at = state.active_turn.as_ref().map(|_| Instant::now());
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

    /// Reconstructs a loop from the event store without executing any work.
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

    /// Reconstructs a loop using a durable checkpoint when it is available.
    ///
    /// Checkpoints are an optimization over the event log, so a missing,
    /// corrupt, or stale snapshot transparently falls back to a full replay.
    /// The resulting state is still validated against the event store's final
    /// sequence before the loop is returned.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_with_checkpoint(
        provider: Arc<P>,
        executor: Arc<E>,
        tools: Arc<ToolRegistry>,
        store: Arc<S>,
        session_id: SessionId,
        checkpoint_path: impl AsRef<Path>,
        policy: Arc<dyn ToolPolicy>,
        config: AgentLoopConfig,
    ) -> Result<Self, HarnessError> {
        let checkpoint_path = checkpoint_path.as_ref();
        let state = replay_with_checkpoint_fallback(checkpoint_path, store.as_ref())
            .await
            .map_err(HarnessError::from)?;
        if state.session_id != session_id {
            return Err(HarnessError::InvariantViolation(format!(
                "event store belongs to session {}, requested {}",
                state.session_id, session_id
            )));
        }
        let last_seq = store.last_seq().await?;
        if state.last_seq != last_seq {
            return Err(HarnessError::InvariantViolation(
                "event store changed while restoring session".into(),
            ));
        }
        // A successful resume is also the safe point to upgrade a legacy
        // snapshot.  Write the replayed state, rather than the possibly stale
        // v1 contents, so the migrated checkpoint covers the same log prefix
        // that was validated above.  A missing or corrupt checkpoint is
        // intentionally left alone; full replay remains the fallback.
        if let Ok(checkpoint) = crate::durable::Checkpoint::read(checkpoint_path).await
            && checkpoint.schema_version != crate::durable::Checkpoint::CURRENT_SCHEMA_VERSION
        {
            crate::durable::Checkpoint::from_state(&state)
                .write_atomic(checkpoint_path)
                .await
                .map_err(HarnessError::from)?;
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

    /// Persists the current reducer state and advances it through the durable
    /// checkpoint marker that was appended to the event log.
    pub async fn create_checkpoint(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<crate::durable::Checkpoint, HarnessError> {
        let checkpoint = create_checkpoint(self.store.as_ref(), &self.state, path)
            .await
            .map_err(HarnessError::from)?;
        self.state = checkpoint.state.clone();
        Ok(checkpoint)
    }
}
