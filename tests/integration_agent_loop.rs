use std::sync::Arc;

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    policy::AllowAllPolicy,
    runtime::session::Session,
    runtime::{SessionStatus, ToolCallId, ToolName},
    tools::{BashTool, EditTool, ToolRegistry},
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn edit_then_bash_returns_results_to_followup_model_requests() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "before")
        .await
        .unwrap();
    let edit_call = ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("edit".into()),
        input: serde_json::json!({
            "path": "note.txt",
            "old_text": "before",
            "new_text": "after"
        }),
    };
    let command = if cfg!(windows) {
        "type note.txt"
    } else {
        "cat note.txt"
    };
    let bash_call = ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("bash".into()),
        input: serde_json::json!({"command": command}),
    };
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(edit_call),
        MockResponse::ToolCall(bash_call),
        MockResponse::Text("finished".into()),
    ]));
    let mut registry = ToolRegistry::default();
    registry.register(EditTool).unwrap();
    registry.register(BashTool::default()).unwrap();
    let mut session = Session::new_with_policy(
        provider.clone(),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        Arc::new(InMemoryEventStore::default()),
        Arc::new(AllowAllPolicy),
    );

    let (_, text) = session
        .start_turn("update and verify".into())
        .await
        .unwrap();

    assert_eq!(text, "finished");
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("note.txt"))
            .await
            .unwrap(),
        "after"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].history.iter().any(|item| {
        matches!(item, mini_harness::runtime::HistoryItem::Tool { name, result, .. }
            if name.0 == "edit" && result.0.contains("new_hash"))
    }));
    assert!(requests[2].history.iter().any(|item| {
        matches!(item, mini_harness::runtime::HistoryItem::Tool { name, result, .. }
            if name.0 == "bash" && result.0.contains("after"))
    }));
    assert_eq!(session.state().status, SessionStatus::Idle);
    assert!(session.state().active_turn.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_bash_turn_kills_the_child_process_group() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("cancel-marker");
    let command = format!(
        "(sleep 30; echo alive > '{}') & wait",
        marker.to_string_lossy()
    );
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("bash".into()),
        input: serde_json::json!({"command": command}),
    })]));
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut loop_ = mini_harness::runtime::agent_loop::AgentLoop::new_with_policy(
        mini_harness::runtime::SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
        Arc::new(AllowAllPolicy),
    );
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        loop_
            .run_turn_with_cancel("run command".into(), task_cancel)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(mini_harness::error::HarnessError::Cancelled)
    ));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!marker.exists());
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TurnCancelled))
    );
}

#[tokio::test]
async fn turn_timeout_cancels_the_active_turn() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::Delay(
        std::time::Duration::from_secs(5),
    )]));
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        store.clone(),
    );

    assert!(matches!(
        session
            .start_turn_with_timeout("slow".into(), std::time::Duration::from_millis(50))
            .await,
        Err(mini_harness::error::HarnessError::Timeout)
    ));
    assert_eq!(session.state().status, SessionStatus::Idle);
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TurnTimedOut))
    );
}
