use std::{path::Path, sync::Arc};

use mini_harness::{
    HarnessError,
    durable::{EventPayload, EventStore, InMemoryEventStore, ToolOutcome, reduce},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    policy::{AllowAllPolicy, DefaultPolicy, ToolPolicy},
    runtime::{
        EventSeq, HistoryItem, SessionId, SessionState, ToolCallId, ToolName,
        agent_loop::{AgentLoop, AgentLoopConfig},
    },
    tools::{BashTool, ToolRegistry},
};
use serde_json::json;
use tempfile::tempdir;

fn call(name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new(),
        name: ToolName(name.into()),
        input,
    }
}

fn agent(
    workspace: &Path,
    responses: Vec<MockResponse>,
    policy: Arc<dyn ToolPolicy>,
    max_steps: usize,
) -> AgentLoop<MockProvider, LocalExecutor, InMemoryEventStore> {
    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    AgentLoop::new_with_config(
        SessionId::new(),
        Arc::new(MockProvider::new(responses)),
        Arc::new(LocalExecutor::new(workspace.to_path_buf())),
        Arc::new(registry),
        Arc::new(InMemoryEventStore::default()),
        policy,
        AgentLoopConfig {
            max_steps,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn malformed_and_unknown_calls_return_feedback_for_correction_and_replay() {
    let directory = tempdir().unwrap();
    let malformed = call("bash", json!({}));
    let unknown = call("missing-tool", json!({}));
    let corrected = call("bash", json!({"command": "printf corrected > result.txt"}));
    let mut agent = agent(
        directory.path(),
        vec![
            MockResponse::ToolCalls(vec![malformed.clone(), unknown.clone()]),
            MockResponse::ToolCall(corrected.clone()),
            MockResponse::Text("done".into()),
        ],
        Arc::new(AllowAllPolicy),
        3,
    );

    assert_eq!(
        agent.run_turn("create result".into()).await.unwrap().1,
        "done"
    );
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("result.txt"))
            .await
            .unwrap(),
        "corrected"
    );
    let requests = agent.provider.requests();
    assert_eq!(requests.len(), 3);
    for (call_id, expected_error) in [
        (malformed.call_id, "command must be a string"),
        (unknown.call_id, "unknown tool"),
    ] {
        assert!(requests[1].history.iter().any(|item| matches!(
            item,
            HistoryItem::Tool { call_id: id, result, .. }
                if *id == call_id && result.0.contains(expected_error)
        )));
    }
    let events = agent.store.read_from(EventSeq(1)).await.unwrap();
    // 只有真正执行的调用才会有成功的 tool.completed（malformed/unknown
    // 被拒绝且未执行）。
    let executed = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCompleted {
                call_id,
                outcome: ToolOutcome::Succeeded { .. },
                ..
            } => Some(*call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(executed, vec![corrected.call_id]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ToolCompleted {
                    outcome: ToolOutcome::Rejected { .. },
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::TurnFailed { .. }))
    );
    let mut replayed = SessionState::new(agent.state.session_id);
    for event in &events {
        reduce(&mut replayed, event).unwrap();
    }
    assert_eq!(replayed, agent.state);
}

#[tokio::test]
async fn malformed_call_does_not_discard_other_calls_in_the_batch() {
    let directory = tempdir().unwrap();
    let valid = call("bash", json!({"command": "printf batch > result.txt"}));
    let mut agent = agent(
        directory.path(),
        vec![
            MockResponse::ToolCalls(vec![call("bash", json!({})), valid.clone()]),
            MockResponse::Text("done".into()),
        ],
        Arc::new(AllowAllPolicy),
        2,
    );

    agent.run_turn("process batch".into()).await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("result.txt"))
            .await
            .unwrap(),
        "batch"
    );
    let requests = agent.provider.requests();
    assert!(requests[1].history.iter().any(|item| matches!(
        item,
        HistoryItem::Tool { call_id, result, .. }
            if *call_id == valid.call_id && result.0.contains("\"exit_code\":0")
    )));
}

#[tokio::test]
async fn repeated_invalid_calls_stop_at_the_step_limit_without_execution() {
    let directory = tempdir().unwrap();
    let mut agent = agent(
        directory.path(),
        (0..3)
            .map(|_| MockResponse::ToolCall(call("bash", json!({}))))
            .collect(),
        Arc::new(AllowAllPolicy),
        2,
    );

    assert!(matches!(
        agent.run_turn("invalid arguments".into()).await,
        Err(HarnessError::Provider(error)) if error.to_string().contains("2 step limit")
    ));
    assert_eq!(agent.provider.requests().len(), 2);
    let events = agent.store.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ToolCompleted {
                    outcome: ToolOutcome::Rejected { .. },
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(matches!(
        events.last().unwrap().payload,
        EventPayload::TurnFailed { .. }
    ));
}

#[tokio::test]
async fn corrected_call_still_requires_policy_approval() {
    let directory = tempdir().unwrap();
    let corrected = call("bash", json!({"command": "touch should-not-exist"}));
    let mut agent = agent(
        directory.path(),
        vec![
            MockResponse::ToolCall(call("bash", json!({}))),
            MockResponse::ToolCall(corrected.clone()),
        ],
        Arc::new(DefaultPolicy),
        3,
    );

    assert!(matches!(
        agent.run_turn("approval required".into()).await,
        Err(HarnessError::ApprovalPending(_))
    ));
    assert!(!directory.path().join("should-not-exist").exists());
    let events = agent.store.read_from(EventSeq(1)).await.unwrap();
    // 审批请求 = requires_approval=true 的 tool.requested。
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolRequested { call_id, requires_approval: true, .. }
            if *call_id == corrected.call_id
    )));
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCompleted {
            outcome: ToolOutcome::Succeeded { .. },
            ..
        }
    )));
}
