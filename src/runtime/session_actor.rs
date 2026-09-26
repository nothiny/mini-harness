use super::{
    agent_loop::AgentLoop,
    ids::TurnId,
    session::{Outcome, OutcomeReply, PendingStart, SessionCommand},
};
use crate::{durable::EventStore, error::HarnessError, executor::Executor, model::ModelProvider};
use std::{collections::VecDeque, sync::Arc};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

const MAX_RETAINED_OUTCOMES: usize = 32;

/// Stable error-classification used only for log lines; protocol callers
/// continue to receive the full typed error.
fn error_kind(error: &HarnessError) -> &'static str {
    match error {
        HarnessError::Config(_) => "config",
        HarnessError::Durable(_) => "durable",
        HarnessError::Provider(_) => "provider",
        HarnessError::Tool(_) => "tool",
        HarnessError::Execution(_) => "execution",
        HarnessError::Policy(_) => "policy",
        HarnessError::ApprovalPending(_) => "approval_pending",
        HarnessError::Cancelled => "cancelled",
        HarnessError::Timeout => "timeout",
        HarnessError::InvariantViolation(_) => "invariant_violation",
        HarnessError::TurnAlreadyActive => "turn_already_active",
        HarnessError::QueueLimitExceeded => "queue_limit_exceeded",
    }
}

pub(super) async fn run_actor<P, E, S>(
    mut receiver: mpsc::Receiver<SessionCommand>,
    mut loop_: AgentLoop<P, E, S>,
) where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let mut outcomes: VecDeque<(TurnId, Outcome)> = VecDeque::new();
    // Inputs accepted while a turn runs: (pre-assigned turn id, input text).
    // The input itself is durable the moment it is accepted; this binding
    // only says which id the actor will use when the turn starts.
    let mut queue: VecDeque<(TurnId, String)> = VecDeque::new();
    // Waiters for queued turns that have not started yet.
    let mut deferred_waiters: std::collections::HashMap<TurnId, Vec<OutcomeReply>> =
        std::collections::HashMap::new();
    let flush_deferred =
        |outcomes: &VecDeque<(TurnId, Outcome)>,
         deferred: &mut std::collections::HashMap<TurnId, Vec<OutcomeReply>>| {
            flush_deferred_outcomes(outcomes, deferred);
        };
    let mut stopped = false;
    while let Some(command) = receiver.recv().await {
        tracing::debug!(target: "mini_harness::session", command = command_name(&command), "session command received");
        match command {
            SessionCommand::StartTurn { input, reply } => {
                if stopped {
                    let _ = reply.send(Err(HarnessError::InvariantViolation(
                        "session actor stopped".into(),
                    )));
                    continue;
                }
                if loop_.state.active_turn.is_some() {
                    // Durable queue: record the input now (never dropped
                    // silently), bound the queue, and pre-assign the turn id
                    // the actor will use once the running turn ends.
                    let queued = queue.len() >= loop_.config.max_queued_inputs;
                    if queued {
                        let _ = reply.send(Err(HarnessError::QueueLimitExceeded));
                        continue;
                    }
                    match loop_.queue_input(input.0.clone()).await {
                        Ok(()) => {
                            let turn_id = TurnId::new();
                            queue.push_back((turn_id, input.0));
                            tracing::info!(
                                target: "mini_harness::session",
                                turn_id = %turn_id,
                                queue_len = queue.len(),
                                "input queued behind active turn"
                            );
                            let _ = reply.send(Ok((turn_id, true)));
                        }
                        Err(error) => {
                            let fatal = matches!(
                                &error,
                                HarnessError::Durable(_) | HarnessError::InvariantViolation(_)
                            );
                            let _ = reply.send(Err(error));
                            stopped = fatal;
                        }
                    }
                    continue;
                }
                let turn_id = TurnId::new();
                match loop_.begin_turn(turn_id, input.0).await {
                    Ok(()) => {
                        let _ = reply.send(Ok((turn_id, false)));
                        tracing::info!(target: "mini_harness::session", turn_id = %turn_id, "turn started");
                        let (fatal, shutting_down) = run_active_turn(
                            &mut receiver,
                            &mut loop_,
                            turn_id,
                            &mut outcomes,
                            &mut queue,
                            &mut deferred_waiters,
                        )
                        .await;
                        flush_deferred(&outcomes, &mut deferred_waiters);
                        if fatal {
                            stopped = true;
                        }
                        if shutting_down {
                            if let Some(active) = loop_.state.active_turn.as_ref() {
                                let _ = loop_
                                    .cancel_active_after_shutdown(
                                        active.turn_id,
                                        "turn cleanup timed out during shutdown",
                                    )
                                    .await;
                            }
                            break;
                        }
                        // Auto-run the durable queue: each finished turn
                        // starts the next recorded input in order.
                        if drain_queued_turns(
                            &mut receiver,
                            &mut loop_,
                            &mut outcomes,
                            &mut queue,
                            &mut deferred_waiters,
                            &mut stopped,
                        )
                        .await
                        {
                            if let Some(active) = loop_.state.active_turn.as_ref() {
                                let _ = loop_
                                    .cancel_active_after_shutdown(
                                        active.turn_id,
                                        "turn cleanup timed out during shutdown",
                                    )
                                    .await;
                            }
                            break;
                        }
                    }
                    Err(error) => {
                        // Provider, tool, policy, timeout, and cancellation
                        // failures are scoped to the turn.  The reducer keeps
                        // the terminal `Failed` status for inspection, while
                        // the next user input performs the recovery
                        // transition back to `Idle`.  Only durable or
                        // invariant failures make the actor unsafe to use.
                        let fatal = matches!(
                            &error,
                            HarnessError::Durable(_) | HarnessError::InvariantViolation(_)
                        );
                        let _ = reply.send(Err(error));
                        stopped = fatal;
                    }
                }
            }
            SessionCommand::WaitTurn { turn_id, reply } => {
                if queue.iter().any(|(id, _)| *id == turn_id) {
                    // The turn is queued but has not started yet; reply with
                    // its outcome once it runs to a terminal state.
                    deferred_waiters.entry(turn_id).or_default().push(reply);
                    continue;
                }
                let result = outcomes
                    .iter()
                    .rev()
                    .find(|(id, _)| *id == turn_id)
                    .map(|(_, outcome)| outcome.clone())
                    .unwrap_or_else(|| {
                        Err(Arc::new(HarnessError::InvariantViolation(
                            "unknown turn".into(),
                        )))
                    });
                let _ = reply.send(result);
            }
            SessionCommand::CancelTurn { turn_id, reply } => {
                let result = if loop_
                    .state
                    .active_turn
                    .as_ref()
                    .is_some_and(|turn| turn.turn_id == turn_id)
                {
                    loop_.cancel_turn(turn_id).await
                } else {
                    Err(HarnessError::InvariantViolation(
                        "requested turn is not active".into(),
                    ))
                };
                let _ = reply.send(result);
            }
            SessionCommand::Checkpoint { path, reply } => {
                // The session is idle here (a busy turn defers checkpoints to
                // `run_active_turn`/`run_approved_turn`), so the marker is
                // appended by the single writer at a safe point.
                let _ = reply.send(loop_.create_checkpoint(path).await);
            }
            SessionCommand::QueryState { reply } => {
                let _ = reply.send(super::state::run_state(&loop_.state));
            }
            SessionCommand::ApproveTool {
                turn_id,
                call_id,
                approved,
                reply,
            } => {
                if stopped {
                    let _ = reply.send(Err(HarnessError::InvariantViolation(
                        "session actor stopped".into(),
                    )));
                    continue;
                }
                let (fatal, shutting_down) = run_approved_turn(
                    &mut receiver,
                    &mut loop_,
                    turn_id,
                    call_id,
                    approved,
                    &mut outcomes,
                    &mut queue,
                    &mut deferred_waiters,
                    reply,
                )
                .await;
                if fatal {
                    stopped = true;
                }
                if shutting_down {
                    if let Some(active) = loop_.state.active_turn.as_ref() {
                        let _ = loop_
                            .cancel_active_after_shutdown(
                                active.turn_id,
                                "approved tool cleanup timed out during shutdown",
                            )
                            .await;
                    }
                    break;
                }
                // The approved turn just finished; drain queued inputs the
                // same way a freshly started turn does, so inputs queued
                // during the approval pause cannot stall (or invert order
                // against later submissions).
                if drain_queued_turns(
                    &mut receiver,
                    &mut loop_,
                    &mut outcomes,
                    &mut queue,
                    &mut deferred_waiters,
                    &mut stopped,
                )
                .await
                {
                    if let Some(active) = loop_.state.active_turn.as_ref() {
                        let _ = loop_
                            .cancel_active_after_shutdown(
                                active.turn_id,
                                "turn cleanup timed out during shutdown",
                            )
                            .await;
                    }
                    break;
                }
            }
            SessionCommand::Shutdown { reply } => {
                let result = if let Some(turn) = loop_.state.active_turn.as_ref() {
                    loop_
                        .cancel_active_after_shutdown(turn.turn_id, "session shutdown")
                        .await
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
                break;
            }
        }
    }
    // The actor is stopping; queued turns will never start, so waiters get a
    // terminal answer instead of parking forever. Nothing is dropped
    // silently: the durable `user.input.recorded` facts remain in the log.
    for (_, waiters) in deferred_waiters.drain() {
        for waiter in waiters {
            let _ = waiter.send(Err(Arc::new(HarnessError::Cancelled)));
        }
    }
}

async fn run_active_turn<P, E, S>(
    receiver: &mut mpsc::Receiver<SessionCommand>,
    loop_: &mut AgentLoop<P, E, S>,
    turn_id: TurnId,
    outcomes: &mut VecDeque<(TurnId, Outcome)>,
    queue: &mut VecDeque<(TurnId, String)>,
    deferred: &mut std::collections::HashMap<TurnId, Vec<OutcomeReply>>,
) -> (bool, bool)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let cancel = CancellationToken::new();
    let mut waiters: Vec<OutcomeReply> = Vec::new();
    let mut cancel_replies: Vec<oneshot::Sender<Result<(), HarnessError>>> = Vec::new();
    let mut checkpoints: Vec<(
        std::path::PathBuf,
        oneshot::Sender<Result<crate::durable::Checkpoint, HarnessError>>,
    )> = Vec::new();
    // Inputs submitted during the turn: recorded durably (and only then
    // acknowledged) after the turn future releases the loop borrow.
    let mut pending_starts: Vec<PendingStart> = Vec::new();
    // State queries deferred while the turn future holds the loop state.
    let mut pending_queries: Vec<oneshot::Sender<super::state::SessionRunState>> = Vec::new();
    let mut shutting_down = false;
    let outcome;
    {
        let run = loop_.continue_turn(turn_id, cancel.clone());
        tokio::pin!(run);
        outcome = loop {
            tokio::select! {
                result = &mut run => {
                    break result
                        .map(|(_, text)| text)
                        .map_err(Arc::new);
                }
                command = receiver.recv() => match command {
                    Some(SessionCommand::QueryState { reply }) => {
                        // The turn future holds the state; answer when it
                        // yields. Approval-paused turns are answered live by
                        // the main loop.
                        pending_queries.push(reply);
                    }
                    Some(SessionCommand::Checkpoint { path, reply }) => {
                        // Deferred until the turn reaches a terminal state so the
                        // snapshot never interleaves with turn events.
                        checkpoints.push((path, reply));
                    }
                    Some(SessionCommand::StartTurn { input, reply }) => {
                        // Durable queue: recorded and acknowledged after the
                        // turn future releases the loop (see pending_starts).
                        let turn_id = TurnId::new();
                        pending_starts.push((turn_id, input.0, reply));
                    }
                    Some(SessionCommand::ApproveTool { reply, .. }) => {
                        let _ = reply.send(Err(HarnessError::TurnAlreadyActive));
                    }
                    Some(SessionCommand::CancelTurn {
                        turn_id: requested,
                        reply,
                    }) => {
                        if requested == turn_id {
                            if cancel.is_cancelled() {
                                cancel_replies.push(reply);
                            } else {
                                cancel.cancel();
                                cancel_replies.push(reply);
                            }
                        } else {
                            let _ = reply.send(Err(HarnessError::InvariantViolation(
                                "turn is not active".into(),
                            )));
                        }
                    }
                    Some(SessionCommand::WaitTurn { turn_id: requested, reply }) => {
                        if requested == turn_id {
                            waiters.push(reply);
                        } else if queue.iter().any(|(id, _)| *id == requested) {
                            // A later queued turn: reply when its own outcome
                            // is flushed by the actor loop.
                            deferred.entry(requested).or_default().push(reply);
                        } else {
                            let result = outcomes
                                .iter()
                                .rev()
                                .find(|(id, _)| *id == requested)
                                .map(|(_, outcome)| outcome.clone())
                                .unwrap_or_else(|| {
                                    Err(Arc::new(HarnessError::InvariantViolation(
                                        "unknown turn".into(),
                                    )))
                                });
                            let _ = reply.send(result);
                        }
                    }
                    Some(SessionCommand::Shutdown { reply }) => {
                        cancel.cancel();
                        shutting_down = true;
                        let result = timeout(Duration::from_secs(1), &mut run).await;
                        let (reply_result, outcome) = match result {
                            Ok(result) => (
                                Ok(()),
                                result.map(|(_, text)| text).map_err(Arc::new),
                            ),
                            Err(_) => {
                                (
                                    Err(HarnessError::Timeout),
                                    Err(Arc::new(HarnessError::Timeout)),
                                )
                            }
                        };
                        let _ = reply.send(reply_result);
                        break outcome;
                    }
                    None => {
                        cancel.cancel();
                        shutting_down = true;
                        let result = timeout(Duration::from_secs(1), &mut run).await;
                        let outcome = match result {
                            Ok(result) => result.map(|(_, text)| text).map_err(Arc::new),
                            Err(_) => Err(Arc::new(HarnessError::Timeout)),
                        };
                        break outcome;
                    }
                }
            }
        };
    } // the turn future is dropped here, releasing the mutable borrow
    for reply in pending_queries {
        let _ = reply.send(super::state::run_state(&loop_.state));
    }
    for (pending_turn, input, reply) in pending_starts {
        if queue.len() >= loop_.config.max_queued_inputs {
            let _ = reply.send(Err(HarnessError::QueueLimitExceeded));
            continue;
        }
        match loop_.queue_input(input.clone()).await {
            Ok(()) => {
                queue.push_back((pending_turn, input.clone()));
                tracing::info!(
                    target: "mini_harness::session",
                    turn_id = %pending_turn,
                    queue_len = queue.len(),
                    "input queued after active turn"
                );
                let _ = reply.send(Ok((pending_turn, true)));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }
    let fatal = matches!(
        outcome,
        Err(ref error)
            if matches!(
                error.as_ref(),
                HarnessError::Durable(_) | HarnessError::InvariantViolation(_)
            )
    );
    match &outcome {
        Ok(text) => {
            tracing::info!(target: "mini_harness::session", turn_id = %turn_id, final_len = text.len(), "turn completed")
        }
        Err(error) => {
            tracing::info!(target: "mini_harness::session", turn_id = %turn_id, error = error_kind(error), fatal, "turn ended with error")
        }
    }
    for reply in waiters {
        let _ = reply.send(outcome.clone());
    }
    for reply in cancel_replies {
        let _ = reply.send(Ok(()));
    }
    outcomes.push_back((turn_id, outcome));
    if outcomes.len() > MAX_RETAINED_OUTCOMES {
        outcomes.pop_front();
    }
    for (path, reply) in checkpoints {
        let _ = reply.send(loop_.create_checkpoint(path).await);
    }
    (fatal, shutting_down)
}

#[allow(clippy::too_many_arguments)]
async fn run_approved_turn<P, E, S>(
    receiver: &mut mpsc::Receiver<SessionCommand>,
    loop_: &mut AgentLoop<P, E, S>,
    turn_id: TurnId,
    call_id: super::ids::ToolCallId,
    approved: bool,
    outcomes: &mut VecDeque<(TurnId, Outcome)>,
    queue: &mut VecDeque<(TurnId, String)>,
    deferred: &mut std::collections::HashMap<TurnId, Vec<OutcomeReply>>,
    approval_reply: oneshot::Sender<Result<(TurnId, String), HarnessError>>,
) -> (bool, bool)
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    let cancel = CancellationToken::new();
    let mut waiters: Vec<OutcomeReply> = Vec::new();
    let mut cancel_replies: Vec<oneshot::Sender<Result<(), HarnessError>>> = Vec::new();
    let mut checkpoints: Vec<(
        std::path::PathBuf,
        oneshot::Sender<Result<crate::durable::Checkpoint, HarnessError>>,
    )> = Vec::new();
    let mut pending_starts: Vec<PendingStart> = Vec::new();
    let mut pending_queries: Vec<oneshot::Sender<super::state::SessionRunState>> = Vec::new();
    let mut shutting_down = false;
    let approval_result;
    {
        let run = loop_.approve_tool(turn_id, call_id, approved, cancel.clone());
        tokio::pin!(run);
        approval_result = loop {
            tokio::select! {
                result = &mut run => break result,
                command = receiver.recv() => match command {
                    Some(SessionCommand::QueryState { reply }) => {
                        pending_queries.push(reply);
                    }
                    Some(SessionCommand::Checkpoint { path, reply }) => {
                        checkpoints.push((path, reply));
                    }
                    Some(SessionCommand::StartTurn { input, reply }) => {
                        let turn_id = TurnId::new();
                        pending_starts.push((turn_id, input.0, reply));
                    }
                    Some(SessionCommand::ApproveTool { reply, .. }) => {
                        let _ = reply.send(Err(HarnessError::TurnAlreadyActive));
                    }
                    Some(SessionCommand::CancelTurn {
                        turn_id: requested,
                        reply,
                    }) => {
                        if requested == turn_id {
                            if cancel.is_cancelled() {
                                cancel_replies.push(reply);
                            } else {
                                cancel.cancel();
                                cancel_replies.push(reply);
                            }
                        } else {
                            let _ = reply.send(Err(HarnessError::InvariantViolation(
                                "turn is not active".into(),
                            )));
                        }
                    }
                    Some(SessionCommand::WaitTurn { turn_id: requested, reply }) => {
                        if requested == turn_id {
                            waiters.push(reply);
                        } else if queue.iter().any(|(id, _)| *id == requested) {
                            // Same deferral as every other phase: the queued
                            // turn gets its outcome once it actually runs.
                            deferred.entry(requested).or_default().push(reply);
                        } else {
                            let result = outcomes
                                .iter()
                                .rev()
                                .find(|(id, _)| *id == requested)
                                .map(|(_, outcome)| outcome.clone())
                                .unwrap_or_else(|| {
                                    Err(Arc::new(HarnessError::InvariantViolation(
                                        "unknown turn".into(),
                                    )))
                                });
                            let _ = reply.send(result);
                        }
                    }
                    Some(SessionCommand::Shutdown { reply }) => {
                        cancel.cancel();
                        shutting_down = true;
                        let outcome = match timeout(Duration::from_secs(1), &mut run).await {
                            Ok(result) => result,
                            Err(_) => Err(HarnessError::Timeout),
                        };
                        let reply_result = if outcome.is_ok() {
                            Ok(())
                        } else {
                            Err(HarnessError::Timeout)
                        };
                        let _ = reply.send(reply_result);
                        break outcome;
                    }
                    None => {
                        cancel.cancel();
                        shutting_down = true;
                        let result = timeout(Duration::from_secs(1), &mut run).await;
                        break match result {
                            Ok(result) => result,
                            Err(_) => Err(HarnessError::Timeout),
                        };
                    }
                }
            }
        };
    } // the approval future is dropped here, releasing the mutable borrow
    for reply in pending_queries {
        let _ = reply.send(super::state::run_state(&loop_.state));
    }
    for (pending_turn, input, reply) in pending_starts {
        if queue.len() >= loop_.config.max_queued_inputs {
            let _ = reply.send(Err(HarnessError::QueueLimitExceeded));
            continue;
        }
        match loop_.queue_input(input.clone()).await {
            Ok(()) => {
                queue.push_back((pending_turn, input.clone()));
                let _ = reply.send(Ok((pending_turn, true)));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }
    let wait_outcome = match &approval_result {
        Ok((_, text)) => Ok(text.clone()),
        Err(error) => Err(Arc::new(wait_error(error))),
    };
    tracing::info!(
        target: "mini_harness::session",
        turn_id = %turn_id,
        approved,
        outcome = wait_outcome.as_ref().map(|_| "completed").unwrap_or("error"),
        "approval processed"
    );
    let fatal = matches!(
        &wait_outcome,
        Err(error)
            if matches!(
                error.as_ref(),
                HarnessError::Durable(_) | HarnessError::InvariantViolation(_)
            )
    );
    let _ = approval_reply.send(approval_result);
    for reply in waiters {
        let _ = reply.send(wait_outcome.clone());
    }
    for reply in cancel_replies {
        let _ = reply.send(Ok(()));
    }
    outcomes.push_back((turn_id, wait_outcome));
    if outcomes.len() > MAX_RETAINED_OUTCOMES {
        outcomes.pop_front();
    }
    for (path, reply) in checkpoints {
        let _ = reply.send(loop_.create_checkpoint(path).await);
    }
    (fatal, shutting_down)
}

fn command_name(command: &SessionCommand) -> &'static str {
    match command {
        SessionCommand::StartTurn { .. } => "start_turn",
        SessionCommand::CancelTurn { .. } => "cancel_turn",
        SessionCommand::ApproveTool { .. } => "approve_tool",
        SessionCommand::WaitTurn { .. } => "wait_turn",
        SessionCommand::Checkpoint { .. } => "checkpoint",
        SessionCommand::QueryState { .. } => "query_state",
        SessionCommand::Shutdown { .. } => "shutdown",
    }
}

fn wait_error(error: &HarnessError) -> HarnessError {
    match error {
        HarnessError::Config(error) => HarnessError::Config(error.clone()),
        HarnessError::Durable(error) => HarnessError::InvariantViolation(error.to_string()),
        HarnessError::Provider(error) => HarnessError::Provider(error.clone()),
        HarnessError::Tool(error) => HarnessError::Tool(error.clone()),
        HarnessError::Execution(error) => HarnessError::Execution(error.clone()),
        HarnessError::Policy(error) => HarnessError::Policy(error.clone()),
        HarnessError::ApprovalPending(error) => HarnessError::ApprovalPending(error.clone()),
        HarnessError::Cancelled => HarnessError::Cancelled,
        HarnessError::Timeout => HarnessError::Timeout,
        HarnessError::InvariantViolation(message) => {
            HarnessError::InvariantViolation(message.clone())
        }
        HarnessError::TurnAlreadyActive => HarnessError::TurnAlreadyActive,
        HarnessError::QueueLimitExceeded => HarnessError::QueueLimitExceeded,
    }
}

/// Sends each finished outcome to any waiters that parked on a queued turn.
fn flush_deferred_outcomes(
    outcomes: &VecDeque<(TurnId, Outcome)>,
    deferred: &mut std::collections::HashMap<TurnId, Vec<OutcomeReply>>,
) {
    for (id, outcome) in outcomes.iter() {
        if let Some(waiters) = deferred.remove(id) {
            for waiter in waiters {
                let _ = waiter.send(outcome.clone());
            }
        }
    }
}

/// Auto-runs queued inputs until the queue empties, the session turns fatal,
/// or a shutdown is requested. Called after **every** path that ends a turn
/// (freshly started turns and approved turns alike), so inputs queued during
/// an approval pause cannot stall behind the next client command.
///
/// Returns `true` when a shutdown was requested mid-drain.
async fn drain_queued_turns<P, E, S>(
    receiver: &mut mpsc::Receiver<SessionCommand>,
    loop_: &mut AgentLoop<P, E, S>,
    outcomes: &mut VecDeque<(TurnId, Outcome)>,
    queue: &mut VecDeque<(TurnId, String)>,
    deferred: &mut std::collections::HashMap<TurnId, Vec<OutcomeReply>>,
    stopped: &mut bool,
) -> bool
where
    P: ModelProvider + 'static,
    E: Executor + 'static,
    S: EventStore + 'static,
{
    while !*stopped && loop_.state.active_turn.is_none() {
        let Some((next_turn, _)) = queue.pop_front() else {
            return false;
        };
        tracing::info!(
            target: "mini_harness::session",
            turn_id = %next_turn,
            remaining = queue.len(),
            "starting next queued input"
        );
        match loop_.begin_queued_turn(next_turn).await {
            Ok(()) => {
                let (fatal, shutting_down) =
                    run_active_turn(receiver, loop_, next_turn, outcomes, queue, deferred).await;
                flush_deferred_outcomes(outcomes, deferred);
                if fatal {
                    *stopped = true;
                }
                if shutting_down {
                    return true;
                }
            }
            Err(error) => {
                tracing::error!(
                    target: "mini_harness::session",
                    turn_id = %next_turn,
                    error = %error,
                    "failed to start queued turn"
                );
                // Give waiters a terminal answer instead of leaving them
                // parked forever.
                let fatal = matches!(
                    &error,
                    HarnessError::Durable(_) | HarnessError::InvariantViolation(_)
                );
                outcomes.push_back((next_turn, Err(Arc::new(error))));
                if outcomes.len() > MAX_RETAINED_OUTCOMES {
                    outcomes.pop_front();
                }
                flush_deferred_outcomes(outcomes, deferred);
                *stopped = fatal;
                return false;
            }
        }
    }
    false
}
