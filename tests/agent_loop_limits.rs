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

/// 累积 history 与滑动窗口后的请求使用不同的上限。
///
/// durable history 超过 `max_context_bytes`（1MB）后，只要滑动窗口
/// 裁剪出的请求仍小于限制，对话就必须能继续 —— 事件追加不得因为
/// "审计历史太长"而失败。
#[tokio::test]
async fn large_durable_history_does_not_block_small_context_requests() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello")
        .await
        .unwrap();
    let session = SessionId::new();

    // 1. 构造 durable history 超过 1MB 的事实序列（80 轮 × 每轮 ~32KB）
    let big = "x".repeat(16 * 1024);
    let mut events = Vec::new();
    let mut seq = 0u64;
    let push = |turn: Option<mini_harness::runtime::TurnId>,
                payload: EventPayload,
                events: &mut Vec<mini_harness::durable::Event>,
                seq: &mut u64| {
        *seq += 1;
        let mut event = mini_harness::durable::Event::new(session, turn, payload);
        event.seq = EventSeq(*seq);
        events.push(event);
    };
    push(None, EventPayload::SessionCreated, &mut events, &mut seq);
    for round in 0..80 {
        let turn = mini_harness::runtime::TurnId::new();
        push(
            None,
            EventPayload::UserInputRecorded {
                input: mini_harness::runtime::UserInput(format!("{round}: {big}")),
            },
            &mut events,
            &mut seq,
        );
        push(Some(turn), EventPayload::TurnStarted, &mut events, &mut seq);
        for payload in [
            EventPayload::ModelResponseRecorded {
                text: mini_harness::runtime::ModelText(format!("done {round}: {big}")),
            },
            EventPayload::TurnCompleted {
                text: mini_harness::runtime::ModelText(format!("done {round}: {big}")),
            },
        ] {
            push(Some(turn), payload, &mut events, &mut seq);
        }
    }
    let mut state = mini_harness::runtime::SessionState::new(session);
    for event in &events {
        mini_harness::durable::reduce(&mut state, event).unwrap();
    }
    assert!(
        serde_json::to_vec(&state.history).unwrap().len() > 1024 * 1024,
        "fixture must push durable history past max_context_bytes"
    );
    assert!(state.active_turn.is_none());
    let last_seq = state.last_seq;

    // 2. 用该状态继续对话: 请求经滑动窗口裁剪后远小于 1MB
    let provider = Arc::new(MockProvider::new(vec![MockResponse::Text(
        "still working".into(),
    )]));
    let store = Arc::new(InMemoryEventStore::default());
    {
        use mini_harness::durable::EventStore;
        for event in &events {
            store.append(event.clone()).await.unwrap();
        }
    }
    let mut loop_ = AgentLoop::from_state(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry_with_read(),
        store.clone(),
        state,
        Arc::new(mini_harness::policy::AllowAllPolicy),
        mini_harness::runtime::agent_loop::AgentLoopConfig::default(),
    );
    assert_eq!(loop_.state.last_seq, last_seq);

    let (_, text) = loop_
        .run_turn("continue after a long history".into())
        .await
        .unwrap();
    assert_eq!(text, "still working");

    // 3. 模型请求确实被滑动窗口压到了限制以内
    let requests = loop_.provider.requests();
    let request_bytes = serde_json::to_vec(&requests[0]).unwrap().len();
    assert!(
        request_bytes
            <= mini_harness::runtime::agent_loop::AgentLoopConfig::default().max_context_bytes,
        "sliding window must bound the request: {request_bytes} bytes"
    );
    assert!(requests[0].history.len() < 322);
}
