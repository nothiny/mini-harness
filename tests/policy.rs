use std::sync::Arc;

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    policy::{PolicyDecision, ToolPolicy},
    runtime::session::Session,
    runtime::{ToolCallId, ToolName},
    tools::{BashTool, ReadTool, ToolRegistry},
};
use tempfile::tempdir;

/// A policy that denies every call, used to assert that a harness denial is
/// distinguishable from a user approval response in the durable log.
struct DenyAllPolicy;

impl ToolPolicy for DenyAllPolicy {
    fn decide(
        &self,
        _spec: &mini_harness::tools::ToolSpec,
        _input: &serde_json::Value,
        _workspace: Option<&mini_harness::runtime::WorkspaceRoot>,
    ) -> PolicyDecision {
        PolicyDecision::Deny
    }
}

#[tokio::test]
async fn default_policy_persists_approval_without_starting_execute_tool() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("should-not-exist");
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("bash".into()),
        input: serde_json::json!({
            "command": format!("echo ran > '{}'", marker.to_string_lossy())
        }),
    })]));
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
    );

    let error = session.start_turn("run command".into()).await.unwrap_err();
    assert!(matches!(
        error,
        mini_harness::HarnessError::ApprovalPending(_)
    ));
    assert!(!marker.exists());
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| { matches!(event.payload, EventPayload::ToolApprovalRequested { .. }) })
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolStarted { .. }))
    );
    assert!(session.state().active_turn.is_some());
    let turn = session.state().active_turn.as_ref().unwrap().turn_id;
    session.cancel_turn(turn).await.unwrap();
    assert!(session.state().active_turn.is_none());
}

#[tokio::test]
async fn approval_pauses_and_records_the_rest_of_a_tool_batch() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello")
        .await
        .unwrap();
    let first = ToolCallId::new();
    let deferred = ToolCallId::new();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCalls(vec![
        ToolCall {
            call_id: first,
            name: ToolName("bash".into()),
            input: serde_json::json!({"command": "touch should_wait_marker"}),
        },
        ToolCall {
            call_id: deferred,
            name: ToolName("read".into()),
            input: serde_json::json!({"path": "note.txt"}),
        },
    ])]));
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    registry.register(ReadTool { max_bytes: 100 }).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
    );

    assert!(matches!(
        session.start_turn("batch".into()).await,
        Err(mini_harness::HarnessError::ApprovalPending(_))
    ));
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::ToolBatchDeferred { call_ids, .. }
                if call_ids == &vec![deferred]
        )
    }));
    session
        .cancel_turn(session.state().active_turn.as_ref().unwrap().turn_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn restoring_a_session_reuses_its_state_and_sequence() {
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let provider = Arc::new(MockProvider::new(vec![MockResponse::Text("done".into())]));
    let mut first = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        store.clone(),
    );
    first.start_turn("hello".into()).await.unwrap();
    let session_id = first.state().session_id;
    let last_seq = first.state().last_seq;

    let resumed = Session::resume(
        Arc::new(MockProvider::new(vec![MockResponse::Text("next".into())])),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        store.clone(),
        session_id,
    )
    .await
    .unwrap();
    assert_eq!(resumed.state().session_id, session_id);
    assert_eq!(resumed.state().last_seq, last_seq);
    assert_eq!(store.last_seq().await.unwrap(), last_seq);
}

#[tokio::test]
async fn approval_executes_the_pending_tool_and_keeps_the_turn_budget() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("approved");
    let call_id = ToolCallId::new();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(ToolCall {
            call_id,
            name: ToolName("bash".into()),
            input: serde_json::json!({
                "command": format!("echo approved > '{}'", marker.to_string_lossy())
            }),
        }),
        MockResponse::Text("approved".into()),
    ]));
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
    );

    let error = session
        .start_turn("ask for approval".into())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        mini_harness::HarnessError::ApprovalPending(_)
    ));
    let turn = session.state().active_turn.as_ref().unwrap().turn_id;
    let (completed_turn, text) = session.approve_tool(turn, call_id, true).await.unwrap();

    assert_eq!(completed_turn, turn);
    assert_eq!(text, "approved");
    assert_eq!(
        tokio::fs::read_to_string(marker).await.unwrap().trim(),
        "approved"
    );
    assert!(session.state().active_turn.is_none());
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::ToolApprovalResponded {
                call_id: event_call_id,
                approved: true,
            } if event_call_id == call_id
        )
    }));
}

#[tokio::test]
async fn policy_denial_is_recorded_without_a_fake_approval_response() {
    let directory = tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("bash".into()),
        input: serde_json::json!({"command": "echo nope"}),
    })]));
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new_with_policy(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
        Arc::new(DenyAllPolicy),
    );

    assert!(matches!(
        session.start_turn("denied".into()).await,
        Err(mini_harness::HarnessError::Policy(_))
    ));
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    // The harness decision is explicit and no approval was ever requested
    // or answered on its behalf.
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolPolicyDenied { reason, .. }
            if reason.contains("policy denied tool")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolApprovalRequested { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolApprovalResponded { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TurnFailed { .. }))
    );
    assert!(session.state().active_turn.is_none());
}

#[tokio::test]
async fn step_budget_fails_the_turn_with_a_durable_terminal_event() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello")
        .await
        .unwrap();
    let call = ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName("read".into()),
        input: serde_json::json!({"path": "note.txt"}),
    };
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(call),
        MockResponse::Text("never reached".into()),
    ]));
    let mut registry = ToolRegistry::default();
    registry
        .register(mini_harness::tools::ReadTool { max_bytes: 100 })
        .unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut loop_ = mini_harness::runtime::agent_loop::AgentLoop::new_with_config(
        mini_harness::runtime::SessionId::new(),
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
        Arc::new(mini_harness::policy::AllowAllPolicy),
        mini_harness::runtime::agent_loop::AgentLoopConfig {
            max_steps: 1,
            ..Default::default()
        },
    );

    assert!(matches!(
        loop_.run_turn("read once".into()).await,
        Err(mini_harness::HarnessError::Provider(_))
    ));
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TurnFailed { .. }))
    );
    assert!(loop_.state.active_turn.is_none());
}
