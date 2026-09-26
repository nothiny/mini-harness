//! Scheduler task identity and lifecycle projection (design §12, plan §14
//! option B).
//!
//! A task is one accepted input. Its durable facts are the
//! `user.input.recorded` event (immediate or queued) and the subsequent
//! `turn.*` events. The lifecycle projection itself lives in
//! [`crate::runtime::run_state`] so the session actor can answer state
//! queries without a circular dependency.

pub use crate::runtime::SessionRunState;

// `run_state` is re-exported through `crate::runtime`; keeping it out of the
// glob avoids an unused-import warning for scheduler-internal users.
pub use crate::runtime::run_state as project_run_state;

use crate::runtime::{SessionId, TurnId};

/// Identifies one submitted input across the scheduler API.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SchedulerTask {
    pub session: SessionId,
    pub turn: TurnId,
}

impl SchedulerTask {
    pub fn new(session: SessionId, turn: TurnId) -> Self {
        Self { session, turn }
    }
}

impl std::fmt::Display for SchedulerTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task {}::{}", self.session, self.turn)
    }
}
