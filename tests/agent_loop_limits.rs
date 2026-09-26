//! Regression tests for agent-loop resource limits:
//!
//! * `max_batch_tool_calls` bounds how many tool calls one provider response
//!   may propose before the turn fails.
//! * A zero duration limit disables that budget instead of expiring the turn
//!   on its first check (`max_turn_duration = 0`, `max_tool_time = 0`).

use std::sync::Arc;

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    runtime::{EventSeq, SessionId, ToolCallId, ToolName, agent_loop::AgentLoop},
    tools::{ReadTool, ToolRegistry},
};
use tempfile::tempdir;

fn read_call(path: &str) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("read".into()),
        input: serde_json::json!({"path": path}),
    }
}

fn registry_with_read() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::default();
    registry.register(ReadTool { max_bytes: 100 }).unwrap();
    Arc::new(registry)
}

#[tokio::test]
async fn oversized_tool_batch_fails_before_any_tool_requested_event() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello")
        .await
        .unwrap();
    let calls = (0..17).map(|_| read_call("note.txt")).collect::<Vec<_>>();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCalls(
        calls.clone(),
    )]));
    let store = Arc::new(InMemoryEventStore::default());
    let mut loop_ = AgentLoop::new_with_config(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry_with_read(),
        store.clone(),
        Arc::new(mini_harness::policy::AllowAllPolicy),
        mini_harness::runtime::agent_loop::AgentLoopConfig::default(),
    );

    assert!(matches!(
        loop_.run_turn("too many calls".into()).await,
        Err(mini_harness::HarnessError::Provider(_))
    ));
    let events = store.read_from(EventSeq(1)).await.unwrap();
    // The batch is rejected before any per-call fact is appended, so neither
    // executions nor tool inputs can grow without bound.
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolRequested { .. }))
    );
    assert!(matches!(
        events.last().map(|event| &event.payload),
        Some(EventPayload::TurnFailed { error, .. }) if error.contains("call limit")
    ));
    assert!(loop_.state.active_turn.is_none());
}

#[tokio::test]
async fn batches_up_to_the_limit_are_still_accepted() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello")
        .await
        .unwrap();
    let calls = (0..16).map(|_| read_call("note.txt")).collect::<Vec<_>>();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(calls),
        MockResponse::Text("done".into()),
    ]));
    let store = Arc::new(InMemoryEventStore::default());
    let mut loop_ = AgentLoop::new_with_config(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry_with_read(),
        store.clone(),
        Arc::new(mini_harness::policy::AllowAllPolicy),
        mini_harness::runtime::agent_loop::AgentLoopConfig::default(),
    );

    let (_, text) = loop_.run_turn("exact limit".into()).await.unwrap();
    assert_eq!(text, "done");
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ToolCompleted { .. }))
            .count(),
        16
    );
}

#[tokio::test]
async fn zero_duration_limits_disable_the_budgets() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello")
        .await
        .unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(read_call("note.txt")),
        MockResponse::Text("done".into()),
    ]));
    let store = Arc::new(InMemoryEventStore::default());
    let mut loop_ = AgentLoop::new_with_config(
        SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry_with_read(),
        store.clone(),
        Arc::new(mini_harness::policy::AllowAllPolicy),
        mini_harness::runtime::agent_loop::AgentLoopConfig {
            max_turn_duration: std::time::Duration::ZERO,
            max_tool_time: std::time::Duration::ZERO,
            ..Default::default()
        },
    );

    // Before this fix, a zero tool budget made `bounded_tool_deadline` fire
    // immediately and the turn ended in `tool.outcome_unknown` + timeout.
    let (_, text) = loop_.run_turn("no budgets".into()).await.unwrap();
    assert_eq!(text, "done");
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolCompleted { .. }))
    );
    assert!(matches!(
        events.last().map(|event| &event.payload),
        Some(EventPayload::TurnCompleted { .. })
    ));
}
