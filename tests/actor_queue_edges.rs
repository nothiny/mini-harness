//! Edge-case probes for the durable input queue around approval pauses.
//!
//! These tests exist because the queue interacts with three async phases:
//! turn execution, approval pause, and approval execution. The drain loop
//! and deferred waiters must behave identically no matter which phase the
//! session is in when an input is queued.

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    policy::{DefaultPolicy, ToolPolicy},
    runtime::{
        EventSeq, SessionId, ToolCallId, ToolName, UserInput, agent_loop::AgentLoopConfig, session,
    },
    tools::{BashTool, ToolRegistry},
};
use std::{sync::Arc, time::Duration};
use tempfile::tempdir;

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    Arc::new(registry)
}

fn bash_call(command: &str) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("bash".into()),
        input: serde_json::json!({ "command": command }),
    }
}

async fn pending_approval_call(store: &InMemoryEventStore) -> ToolCallId {
    let events = store.read_from(EventSeq(1)).await.unwrap();
    events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolApprovalRequested { call_id } => Some(*call_id),
            _ => None,
        })
        .expect("an approval request must be recorded")
}

/// Queued while a turn is approval-paused, the input must auto-run once the
/// approval resolves and the turn reaches a terminal state.
#[tokio::test]
async fn queued_input_auto_runs_after_the_approval_turn_completes() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(bash_call("touch one")),
        MockResponse::Text("one done".into()),
        MockResponse::Text("two done".into()),
    ]));
    let (handle, _actor) = session::spawn_with_policy_id(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
    );

    let (first, queued) = handle
        .start_turn_with_status(UserInput("one".into()))
        .await
        .unwrap();
    assert!(!queued);
    // The turn pauses durably on the approval request.
    assert!(matches!(
        handle.wait_turn(first).await,
        Err(ref error) if matches!(error.as_ref(), mini_harness::HarnessError::ApprovalPending(_))
    ));
    let call = pending_approval_call(&store).await;
    let (second, queued) = handle
        .start_turn_with_status(UserInput("two".into()))
        .await
        .unwrap();
    assert!(queued, "input submitted during the pause must queue");

    // Approve; the paused turn must finish…
    let (turn, text) = handle.approve_tool(first, call, true).await.unwrap();
    assert_eq!(turn, first);
    assert_eq!(text, "one done");
    // …and the queued input must then run on its own, without any further
    // command from the client.
    let second_outcome = tokio::time::timeout(Duration::from_secs(5), handle.wait_turn(second))
        .await
        .expect("queued input must auto-run after the approval turn")
        .unwrap();
    assert_eq!(second_outcome, "two done");
    assert_eq!(
        handle.query_state().await.unwrap(),
        mini_harness::scheduler::SessionRunState::Idle
    );
}

/// FIFO must survive the mixed path: queued during a pause, then a fresh
/// submit after the pause resolves — the queued input still runs first.
#[tokio::test]
async fn queued_input_keeps_fifo_priority_over_later_submissions() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(bash_call("touch one")),
        MockResponse::Text("one done".into()),
        MockResponse::Text("two done".into()),
        MockResponse::Text("three done".into()),
    ]));
    let (handle, _actor) = session::spawn_with_policy_id(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
    );

    let (first, _) = handle
        .start_turn_with_status(UserInput("one".into()))
        .await
        .unwrap();
    assert!(matches!(
        handle.wait_turn(first).await,
        Err(ref error) if matches!(error.as_ref(), mini_harness::HarnessError::ApprovalPending(_))
    ));
    let call = pending_approval_call(&store).await;
    let (second, queued) = handle
        .start_turn_with_status(UserInput("two".into()))
        .await
        .unwrap();
    assert!(queued);

    let approver = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.approve_tool(first, call, true).await })
    };
    approver.await.unwrap().unwrap();
    // A fresh submission after the queue already holds `two`.
    let (third, queued) = handle
        .start_turn_with_status(UserInput("three".into()))
        .await
        .unwrap();
    let _ = queued;

    let second_outcome = tokio::time::timeout(Duration::from_secs(5), handle.wait_turn(second))
        .await
        .expect("queued input must run before the later submission's turn")
        .unwrap();
    let third_outcome = tokio::time::timeout(Duration::from_secs(5), handle.wait_turn(third))
        .await
        .expect("the later submission must also run")
        .unwrap();
    assert_eq!(second_outcome, "two done");
    assert_eq!(third_outcome, "three done");

    // Durable proof of FIFO: turn.started for `second` precedes `third`.
    let events = store.read_from(EventSeq(1)).await.unwrap();
    let started = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::TurnStarted => event.turn_id,
            _ => None,
        })
        .collect::<Vec<_>>();
    let second_index = started
        .iter()
        .position(|id| *id == second)
        .expect("second turn must start");
    let third_index = started
        .iter()
        .position(|id| *id == third)
        .expect("third turn must start");
    assert!(
        second_index < third_index,
        "queued input must start before the later submission (started order: {started:?})"
    );
}

/// A wait on a queued turn that arrives while the *approval execution* phase
/// is still running must defer (like every other phase), not error with
/// "unknown turn".
#[tokio::test]
async fn wait_on_a_queued_turn_defers_during_approval_execution() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(bash_call("touch one_later")),
        MockResponse::Text("one done".into()),
        MockResponse::Text("two done".into()),
    ]));
    let (handle, _actor) = session::spawn_with_policy_id(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
    );

    let (first, _) = handle
        .start_turn_with_status(UserInput("one".into()))
        .await
        .unwrap();
    assert!(matches!(
        handle.wait_turn(first).await,
        Err(ref error) if matches!(error.as_ref(), mini_harness::HarnessError::ApprovalPending(_))
    ));
    let call = pending_approval_call(&store).await;
    let (second, queued) = handle
        .start_turn_with_status(UserInput("two".into()))
        .await
        .unwrap();
    assert!(queued);

    // Approve in the background; the approved bash sleeps 400ms, so the wait
    // below lands inside the approval-execution phase.
    let approver = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.approve_tool(first, call, true).await })
    };
    tokio::time::sleep(Duration::from_millis(80)).await;
    let second_outcome = tokio::time::timeout(Duration::from_secs(5), handle.wait_turn(second))
        .await
        .expect("wait on a queued turn must defer, not hang forever")
        .unwrap();
    approver.await.unwrap().unwrap();
    // It must be the queued turn's result — not an "unknown turn" error.
    assert_eq!(second_outcome, "two done");
}

/// Shutdown with a queued input: the durable fact stays in the log and any
/// deferred waiter gets a terminal answer instead of hanging.
#[tokio::test]
async fn shutdown_with_queued_input_answers_waiters_and_keeps_the_fact() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(bash_call(
        "sleep 5",
    ))]));
    let (handle, actor) = session::spawn_with_policy_id(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy) as Arc<dyn ToolPolicy>,
    );

    let (first, _) = handle
        .start_turn_with_status(UserInput("one".into()))
        .await
        .unwrap();
    let _ = first;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (second, queued) = handle
        .start_turn_with_status(UserInput("two".into()))
        .await
        .unwrap();
    assert!(queued);

    let waiter = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.wait_turn(second).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle.shutdown().await;
    actor.await.unwrap();

    // The waiter must receive a terminal answer within a bounded time.
    let outcome = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("deferred waiter must be answered at shutdown")
        .unwrap();
    assert!(
        outcome.is_err(),
        "a queued turn that never started must not report success"
    );

    // And the durable fact survives: the log contains the queued input even
    // though its turn never started.
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserInputRecorded { input } if input.0 == "two"
        )),
        "queued input must remain a durable fact after shutdown"
    );
    let _ = AgentLoopConfig::default();
}

/// Steering semantics, pinned as tests: a queued input becomes durable the
/// moment the actor can append it. That is immediately while the turn is
/// approval-paused (so the resumed sampling *sees* the new input), and at
/// the next turn boundary while a turn is executing (the running turn does
/// not see it; the *next* turn does). Both behaviours are load-bearing and
/// must not drift silently.

#[tokio::test]
async fn queued_input_steers_the_turn_that_resumes_after_approval() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(bash_call("touch one")),
        MockResponse::Text("one done".into()),
    ]));
    let provider_handle = Arc::clone(&provider);
    let (handle, _actor) = session::spawn_with_policy_id(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
    );

    let (first, _) = handle
        .start_turn_with_status(UserInput("one".into()))
        .await
        .unwrap();
    assert!(matches!(
        handle.wait_turn(first).await,
        Err(ref error) if matches!(error.as_ref(), mini_harness::HarnessError::ApprovalPending(_))
    ));
    let call = pending_approval_call(&store).await;

    // Queued while the turn is paused: recorded immediately.
    let (_steered, queued) = handle
        .start_turn_with_status(UserInput("steer me".into()))
        .await
        .unwrap();
    assert!(queued);

    handle.approve_tool(first, call, true).await.unwrap();

    // The resumed sampling must have seen the steered input in its history.
    let requests = provider_handle.requests();
    let resumed = requests
        .last()
        .expect("approval resumption must sample again");
    assert!(
        resumed.history.iter().any(|item| matches!(
            item,
            mini_harness::runtime::HistoryItem::User(input) if input.0 == "steer me"
        )),
        "steer input must be visible to the resumed sampling: {:?}",
        resumed.history
    );
}

#[tokio::test]
async fn queued_input_during_execution_becomes_visible_to_the_next_turn() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Delay(std::time::Duration::from_millis(300)),
        MockResponse::Text("one done".into()),
        MockResponse::Text("two done".into()),
    ]));
    let provider_handle = Arc::clone(&provider);
    let (handle, _actor) = session::spawn_with_policy_id(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
    );

    let (first, _) = handle
        .start_turn_with_status(UserInput("one".into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (second, queued) = handle
        .start_turn_with_status(UserInput("late steer".into()))
        .await
        .unwrap();
    assert!(queued);

    // The running turn finishes without ever seeing the late input…
    assert_eq!(handle.wait_turn(first).await.unwrap(), "one done");
    let requests = provider_handle.requests();
    let first_request = &requests[0];
    assert!(
        !first_request.history.iter().any(|item| matches!(
            item,
            mini_harness::runtime::HistoryItem::User(input) if input.0 == "late steer"
        )),
        "an input submitted mid-execution cannot retroactively enter the running turn's context"
    );
    // …and the queued input runs as its own turn, whose context includes it.
    assert_eq!(handle.wait_turn(second).await.unwrap(), "two done");
    let requests = provider_handle.requests();
    let second_request = requests.last().unwrap();
    assert!(
        second_request.history.iter().any(|item| matches!(
            item,
            mini_harness::runtime::HistoryItem::User(input) if input.0 == "late steer"
        )),
        "the queued input must be visible to its own turn"
    );
}
