use std::sync::Arc;
use std::time::Duration;

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse},
    runtime::{EventSeq, SessionId, TurnId, session::Session},
    tools::ToolRegistry,
};
use tempfile::tempdir;

#[tokio::test]
async fn cancel_turn_acknowledges_only_the_active_turn() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        Arc::new(MockProvider::new(vec![MockResponse::Delay(
            Duration::from_secs(5),
        )])),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        Arc::clone(&store),
    );
    let session_id = session.state().session_id;
    let turn = session
        .start_turn_with_timeout("cancel me".into(), Duration::from_millis(100))
        .await;
    assert!(matches!(
        turn,
        Err(mini_harness::HarnessError::Timeout | mini_harness::HarnessError::Cancelled)
    ));
    // The turn is already terminal; cancelling it again is rejected.
    let finished_turn = turn_terminal_id(&store, session_id).await;
    assert!(session.cancel_turn(finished_turn).await.is_err());
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::TurnCancelled | EventPayload::TurnTimedOut
    )));
}

async fn turn_terminal_id<S: EventStore>(
    store: &Arc<S>,
    session_id: SessionId,
) -> mini_harness::runtime::TurnId {
    let events = store.read_from(EventSeq(1)).await.unwrap();
    events
        .iter()
        .find(|event| event.session_id == session_id)
        .and_then(|event| event.turn_id)
        .unwrap_or_else(TurnId::new)
}

#[tokio::test]
async fn session_can_approve_a_pending_tool_after_the_policy_outcome() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("session-approved");
    let call_id = mini_harness::runtime::ToolCallId::new();
    let mut tools = ToolRegistry::default();
    tools
        .register(mini_harness::tools::BashTool::default())
        .unwrap();
    let mut session = Session::new(
        Arc::new(MockProvider::new(vec![
            mini_harness::model::MockResponse::ToolCall(mini_harness::model::ToolCall {
                call_id,
                name: mini_harness::runtime::ToolName("bash".into()),
                input: serde_json::json!({
                    "command": format!("echo approved > '{}'", marker.to_string_lossy())
                }),
            }),
            MockResponse::Text("approved".into()),
        ])),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(tools),
        Arc::new(InMemoryEventStore::default()),
    );

    let error = session.start_turn("approve this".into()).await.unwrap_err();
    assert!(matches!(
        error,
        mini_harness::HarnessError::ApprovalPending(_)
    ));
    let turn = session.state().active_turn.as_ref().unwrap().turn_id;
    let (completed_turn, text) = session.approve_tool(turn, call_id, true).await.unwrap();
    assert_eq!(completed_turn, turn);
    assert_eq!(text, "approved");
    assert!(marker.exists());
}
