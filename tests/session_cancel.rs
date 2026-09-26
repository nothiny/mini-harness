use std::sync::Arc;
use std::time::Duration;

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    runtime::session::spawn,
    runtime::{TurnId, UserInput, session},
    tools::{BashTool, ToolRegistry},
};
use tempfile::tempdir;

#[tokio::test]
async fn cancel_turn_acknowledges_only_the_active_turn() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let (handle, actor) = session::spawn(
        Arc::new(MockProvider::new(vec![MockResponse::Delay(
            Duration::from_secs(5),
        )])),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        Arc::clone(&store),
    );

    assert!(handle.cancel_turn(TurnId::new()).await.is_err());
    let turn = handle
        .start_turn(UserInput("cancel me".into()))
        .await
        .unwrap();
    assert!(handle.cancel_turn(TurnId::new()).await.is_err());
    assert!(handle.cancel_turn(turn).await.is_ok());
    assert!(
        matches!(handle.wait_turn(turn).await, Err(error) if matches!(&*error, mini_harness::HarnessError::Cancelled))
    );
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    let cancel_events = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::TurnCancelRequested))
        .count();
    assert_eq!(cancel_events, 1);
    assert!(handle.shutdown_with_result().await.is_ok());
    actor.await.unwrap();
}

#[tokio::test]
async fn actor_can_approve_a_pending_tool_after_the_policy_outcome() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("actor-approved");
    let call_id = mini_harness::runtime::ToolCallId::new();
    let (handle, actor) = spawn(
        Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall(ToolCall {
                call_id,
                name: mini_harness::runtime::ToolName("bash".into()),
                input: serde_json::json!({
                    "command": format!("echo approved > '{}'", marker.to_string_lossy())
                }),
            }),
            MockResponse::Text("approved".into()),
        ])),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new({
            let mut tools = ToolRegistry::default();
            tools.register(BashTool::default()).unwrap();
            tools
        }),
        Arc::new(InMemoryEventStore::default()),
    );

    let turn = handle
        .start_turn(UserInput("approve this".into()))
        .await
        .unwrap();
    assert!(matches!(
        handle.wait_turn(turn).await,
        Err(error) if matches!(&*error, mini_harness::HarnessError::ApprovalPending(_))
    ));
    let (completed_turn, text) = handle.approve_tool(turn, call_id, true).await.unwrap();
    assert_eq!(completed_turn, turn);
    assert_eq!(text, "approved");
    assert!(marker.exists());
    handle.shutdown_with_result().await.unwrap();
    actor.await.unwrap();
}
