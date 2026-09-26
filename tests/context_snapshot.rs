use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ModelRequest, ToolCall},
    runtime::agent_loop::AgentLoop,
    runtime::session::{Session, spawn_with_system_instructions},
    runtime::{
        ContextSnapshot, HistoryItem, ModelText, SessionId, SessionState, ToolCallId, ToolName,
        UserInput,
    },
    tools::{RiskClass, ToolSpec},
};
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName(name.into()),
        description: format!("{name} tool"),
        parameters: json!({"type": "object"}),
        risk: RiskClass::Read,
    }
}

#[tokio::test]
async fn agent_loop_passes_system_instructions_through_snapshot() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::Text("done".into())]));
    let store = Arc::new(InMemoryEventStore::default());
    let mut loop_ = AgentLoop::new(
        SessionId::new(),
        provider.clone(),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(mini_harness::tools::ToolRegistry::default()),
        store,
    );
    loop_.set_system_instructions("be concise");

    loop_.run_turn("hello".into()).await.unwrap();

    assert_eq!(
        provider.requests()[0].system_instructions,
        Some("be concise".into())
    );
}

#[test]
fn context_snapshot_owns_history_and_tool_specs() {
    let session = SessionId::new();
    let mut state = SessionState::new(session);
    state
        .history
        .push(HistoryItem::User(UserInput("hello".into())));
    let mut specs = vec![spec("read")];
    let snapshot = ContextSnapshot::from_state(&state, specs.clone());

    state
        .history
        .push(HistoryItem::Assistant(ModelText("later".into())));
    specs[0].description = "changed after snapshot".into();
    let request = snapshot.into_model_request();

    let expected = ModelRequest {
        model: None,
        system_instructions: None,
        history: vec![HistoryItem::User(UserInput("hello".into()))],
        tools: vec![spec("read")],
        continuation: None,
    };
    assert_eq!(request, expected);
}

#[test]
fn context_snapshot_carries_system_instructions() {
    let session = SessionId::new();
    let state = SessionState::new(session);
    let snapshot =
        ContextSnapshot::from_state_with_system_instructions(&state, vec![], "be concise");

    assert_eq!(snapshot.system_instructions(), Some("be concise"));
    assert!(snapshot.history().is_empty());
    assert!(snapshot.tools().is_empty());
    assert_eq!(
        snapshot.into_model_request().system_instructions,
        Some("be concise".into())
    );
}

#[test]
fn context_snapshot_bounds_each_model_visible_text_item() {
    let session = SessionId::new();
    let mut state = SessionState::new(session);
    state.history.push(HistoryItem::User(UserInput(
        "x".repeat(mini_harness::runtime::context::MAX_CONTEXT_ITEM_BYTES + 100),
    )));

    let snapshot = ContextSnapshot::from_state_with_system_instructions(
        &state,
        vec![],
        "y".repeat(mini_harness::runtime::context::MAX_CONTEXT_ITEM_BYTES + 100),
    );
    let request = snapshot.into_model_request();
    let HistoryItem::User(input) = &request.history[0] else {
        panic!("expected user history item");
    };
    assert!(input.0.len() <= mini_harness::runtime::context::MAX_CONTEXT_ITEM_BYTES);
    assert!(input.0.ends_with("[context item truncated]"));
    assert!(
        request.system_instructions.as_deref().is_some_and(
            |value| value.len() <= mini_harness::runtime::context::MAX_CONTEXT_ITEM_BYTES
        )
    );
}

#[test]
fn context_snapshot_bounds_total_history_copy() {
    let mut state = SessionState::new(SessionId::new());
    for index in 0..64 {
        state.history.push(HistoryItem::User(UserInput(format!(
            "item-{index}:{}",
            "x".repeat(20 * 1024)
        ))));
    }

    let snapshot = ContextSnapshot::from_state(&state, vec![]);
    let encoded = serde_json::to_vec(snapshot.history()).unwrap();

    assert!(snapshot.history().len() < state.history.len());
    assert!(encoded.len() <= mini_harness::runtime::context::MAX_CONTEXT_HISTORY_BYTES);
}

#[tokio::test]
async fn session_forwards_system_instructions_to_provider() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::Text("done".into())]));
    let mut session = Session::new(
        provider.clone(),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(mini_harness::tools::ToolRegistry::default()),
        Arc::new(InMemoryEventStore::default()),
    );
    session.set_system_instructions("be concise");

    session.start_turn("hello".into()).await.unwrap();

    assert_eq!(
        provider.requests()[0].system_instructions,
        Some("be concise".into())
    );
}

#[tokio::test]
async fn actor_forwards_system_instructions_across_turns() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Text("first".into()),
        MockResponse::Text("second".into()),
    ]));
    let (handle, task) = spawn_with_system_instructions(
        provider.clone(),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(mini_harness::tools::ToolRegistry::default()),
        Arc::new(InMemoryEventStore::default()),
        "be concise",
    );

    let first = handle.start_turn(UserInput("one".into())).await.unwrap();
    assert_eq!(handle.wait_turn(first).await.unwrap(), "first");
    let second = handle.start_turn(UserInput("two".into())).await.unwrap();
    assert_eq!(handle.wait_turn(second).await.unwrap(), "second");
    handle.shutdown().await;
    task.await.unwrap();

    assert_eq!(
        provider
            .requests()
            .iter()
            .map(|request| request.system_instructions.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("be concise"), Some("be concise")]
    );
}

#[tokio::test]
async fn completed_model_response_is_not_duplicated_in_next_snapshot() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Text("first".into()),
        MockResponse::Text("second".into()),
    ]));
    let mut session = Session::new(
        provider.clone(),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(mini_harness::tools::ToolRegistry::default()),
        Arc::new(InMemoryEventStore::default()),
    );

    session.start_turn("one".into()).await.unwrap();
    session.start_turn("two".into()).await.unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].history,
        vec![
            HistoryItem::User(UserInput("one".into())),
            HistoryItem::Assistant(ModelText("first".into())),
            HistoryItem::User(UserInput("two".into())),
        ]
    );
    assert_eq!(
        session.state().history,
        vec![
            HistoryItem::User(UserInput("one".into())),
            HistoryItem::Assistant(ModelText("first".into())),
            HistoryItem::User(UserInput("two".into())),
            HistoryItem::Assistant(ModelText("second".into())),
        ]
    );
}

#[tokio::test]
async fn repeated_final_text_is_recorded_for_each_turn() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Text("same".into()),
        MockResponse::Text("same".into()),
    ]));
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(mini_harness::tools::ToolRegistry::default()),
        Arc::new(InMemoryEventStore::default()),
    );

    session.start_turn("one".into()).await.unwrap();
    session.start_turn("two".into()).await.unwrap();

    assert_eq!(
        session.state().history,
        vec![
            HistoryItem::User(UserInput("one".into())),
            HistoryItem::Assistant(ModelText("same".into())),
            HistoryItem::User(UserInput("two".into())),
            HistoryItem::Assistant(ModelText("same".into())),
        ]
    );
}

#[tokio::test]
async fn multiple_tool_calls_are_executed_in_event_order() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("a.txt"), "alpha")
        .await
        .unwrap();
    tokio::fs::write(directory.path().join("b.txt"), "beta")
        .await
        .unwrap();
    let calls = vec![
        ToolCall {
            call_id: ToolCallId::new(),
            name: ToolName("read".into()),
            input: serde_json::json!({"path": "a.txt"}),
        },
        ToolCall {
            call_id: ToolCallId::new(),
            name: ToolName("read".into()),
            input: serde_json::json!({"path": "b.txt"}),
        },
    ];
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCalls(calls),
        MockResponse::Text("both read".into()),
    ]));
    let mut registry = mini_harness::tools::ToolRegistry::default();
    registry
        .register(mini_harness::tools::ReadTool { max_bytes: 100 })
        .unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider.clone(),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
    );

    assert_eq!(
        session.start_turn("read both".into()).await.unwrap().1,
        "both read"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].history.iter().any(|item| {
        matches!(item, HistoryItem::Tool { result, .. } if result.0.contains("alpha"))
    }));
    assert!(requests[1].history.iter().any(|item| {
        matches!(item, HistoryItem::Tool { result, .. } if result.0.contains("beta"))
    }));

    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    // 12 core facts + one provider continuation per tool-call response.
    assert_eq!(events.len(), 14);
    assert!(matches!(
        events[3].payload,
        EventPayload::ProviderContinuationUpdated { .. }
    ));
    assert!(matches!(
        events[4].payload,
        EventPayload::ModelResponseRecorded { .. }
    ));
    assert!(matches!(
        events[5].payload,
        EventPayload::ToolRequested { .. }
    ));
    assert!(matches!(
        events[6].payload,
        EventPayload::ToolStarted { .. }
    ));
    assert!(matches!(
        events[7].payload,
        EventPayload::ToolCompleted { .. }
    ));
    assert!(matches!(
        events[8].payload,
        EventPayload::ToolRequested { .. }
    ));
    assert!(matches!(
        events[9].payload,
        EventPayload::ToolStarted { .. }
    ));
    assert!(matches!(
        events[10].payload,
        EventPayload::ToolCompleted { .. }
    ));
    assert!(matches!(
        events[12].payload,
        EventPayload::ModelResponseRecorded { .. }
    ));
    assert!(matches!(
        events[13].payload,
        EventPayload::TurnCompleted { .. }
    ));
}

#[tokio::test]
async fn duplicate_tool_call_ids_fail_the_turn_before_execution() {
    let call = ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("read".into()),
        input: serde_json::json!({"path": "a.txt"}),
    };
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCalls(vec![
        call.clone(),
        call,
    ])]));
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(mini_harness::tools::ToolRegistry::default()),
        store.clone(),
    );

    assert!(matches!(
        session.start_turn("bad batch".into()).await,
        Err(mini_harness::error::HarnessError::Provider(_))
    ));
    assert_eq!(
        session.state().status,
        mini_harness::runtime::SessionStatus::Failed
    );
    assert!(session.state().active_turn.is_none());
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    // The mock's continuation is recorded before the duplicate batch fails.
    assert_eq!(events.len(), 5);
    assert!(matches!(
        events[3].payload,
        EventPayload::ProviderContinuationUpdated { .. }
    ));
    assert!(matches!(events[4].payload, EventPayload::TurnFailed { .. }));
}
