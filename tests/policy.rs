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

/// A policy that asks for every call, used to drive approval flows.
struct AskAllPolicy;

impl ToolPolicy for AskAllPolicy {
    fn decide(
        &self,
        _spec: &mini_harness::tools::ToolSpec,
        _input: &serde_json::Value,
    ) -> PolicyDecision {
        PolicyDecision::Ask
    }
}

fn requires_approval(event: &mini_harness::durable::Event) -> Option<bool> {
    match &event.payload {
        EventPayload::ToolRequested {
            requires_approval, ..
        } => Some(*requires_approval),
        _ => None,
    }
}

#[tokio::test]
async fn default_policy_persists_approval_request_without_executing() {
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
    // 审批请求就是带 requires_approval=true 的 tool.requested 事件。
    assert!(
        events
            .iter()
            .any(|event| requires_approval(event) == Some(true))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolCompleted { .. }))
    );
    assert!(session.state().active_turn.is_some());
    let turn = session.state().active_turn.as_ref().unwrap().turn_id;
    session.cancel_turn(turn).await.unwrap();
    assert!(session.state().active_turn.is_none());
}

#[tokio::test]
async fn approval_pauses_before_the_rest_of_a_tool_batch() {
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
    // 第一个调用请求了审批；批中后续调用尚未请求（恢复采样后由模型重新发起）。
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::ToolRequested { call_id, requires_approval: true, .. }
                if *call_id == first
        )
    }));
    assert!(!events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::ToolRequested { call_id, .. } if *call_id == deferred
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

/// 简化版没有策略层 Deny：拒绝统一表示为「Ask + 用户拒绝」，
/// 事件日志只记录一种拒绝事实（approved: false）。
#[tokio::test]
async fn user_denial_records_a_single_refusal_fact_and_cancels_the_turn() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("denied-marker");
    let call_id = ToolCallId::new();
    let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(ToolCall {
        call_id,
        name: ToolName("bash".into()),
        input: serde_json::json!({
            "command": format!("echo nope > '{}'", marker.to_string_lossy())
        }),
    })]));
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new_with_policy(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        store.clone(),
        Arc::new(AskAllPolicy),
    );

    assert!(matches!(
        session.start_turn("denied".into()).await,
        Err(mini_harness::HarnessError::ApprovalPending(_))
    ));
    let turn = session.state().active_turn.as_ref().unwrap().turn_id;
    let error = session
        .approve_tool(turn, call_id, false)
        .await
        .unwrap_err();
    assert!(matches!(error, mini_harness::HarnessError::Policy(_)));
    assert!(!marker.exists());
    assert!(session.state().active_turn.is_none());
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::ToolApprovalResponded { call_id: event_call_id, approved: false }
                if *event_call_id == call_id
        )
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolCompleted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TurnCancelled))
    );
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
