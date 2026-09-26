use mini_harness::{
    durable::{
        Event, EventPayload, EventStore, InMemoryEventStore, RecoveryFinding, classify_events,
        inspect_store, policy_for_tool, recover_store,
    },
    error::HarnessError,
    runtime::{
        EventSeq, ExecutionId, ModelText, RecoveryPolicy, SessionId, ToolCallId, ToolName,
        ToolResult, TurnId, UserInput,
    },
};

fn event(session_id: SessionId, turn_id: Option<TurnId>, seq: u64, payload: EventPayload) -> Event {
    let mut event = Event::new(session_id, turn_id, payload);
    event.seq = EventSeq(seq);
    event
}

fn interrupted_tool_events(
    session_id: SessionId,
    turn_id: TurnId,
    call_id: ToolCallId,
    tool_name: &str,
    execution_id: ExecutionId,
) -> Vec<Event> {
    vec![
        event(session_id, None, 1, EventPayload::SessionCreated),
        event(
            session_id,
            None,
            2,
            EventPayload::UserInputRecorded {
                input: UserInput("run tool".into()),
            },
        ),
        event(session_id, Some(turn_id), 3, EventPayload::TurnStarted),
        event(
            session_id,
            Some(turn_id),
            4,
            EventPayload::ModelResponseRecorded {
                text: ModelText(format!("tool call: {tool_name}")),
            },
        ),
        event(
            session_id,
            Some(turn_id),
            5,
            EventPayload::ToolRequested {
                call_id,
                name: ToolName(tool_name.into()),
                input: serde_json::json!({}),
            },
        ),
        event(
            session_id,
            Some(turn_id),
            6,
            EventPayload::ToolStarted {
                call_id,
                execution_id,
            },
        ),
    ]
}

#[test]
fn classifies_running_tool_as_unknown_with_conservative_policy() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let execution_id = ExecutionId::new();
    let report = classify_events(
        session_id,
        &interrupted_tool_events(session_id, turn_id, call_id, "bash", execution_id),
    )
    .unwrap();

    assert!(!report.is_clean());
    assert!(report.action_required());
    assert!(report.findings.iter().any(|finding| {
        matches!(
            finding,
            RecoveryFinding::ActiveTurn { turn_id: id } if *id == turn_id
        )
    }));
    assert!(report.findings.iter().any(|finding| {
        matches!(
            finding,
            RecoveryFinding::UnknownTool {
                call_id: id,
                execution_id: Some(found),
                policy: RecoveryPolicy::RequireUserDecision,
                ..
            } if *id == call_id && *found == execution_id
        )
    }));
}

#[test]
fn classifies_explicit_unknown_without_execution_id() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let mut events =
        interrupted_tool_events(session_id, turn_id, call_id, "edit", ExecutionId::new());
    events.push(event(
        session_id,
        Some(turn_id),
        7,
        EventPayload::ToolOutcomeUnknown {
            call_id,
            reason: "harness stopped after process exit".into(),
        },
    ));
    let report = classify_events(session_id, &events).unwrap();
    assert!(report.findings.iter().any(|finding| {
        matches!(
            finding,
            RecoveryFinding::UnknownTool {
                call_id: id,
                execution_id: None,
                policy: RecoveryPolicy::InspectEditHash,
                ..
            } if *id == call_id
        )
    }));
}

#[test]
fn pending_approval_is_reported_before_execution_starts() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let mut events =
        interrupted_tool_events(session_id, turn_id, call_id, "bash", ExecutionId::new());
    events.pop();
    events.push(event(
        session_id,
        Some(turn_id),
        6,
        EventPayload::ToolApprovalRequested { call_id },
    ));
    let report = classify_events(session_id, &events).unwrap();
    assert!(report.findings.iter().any(|finding| {
        matches!(
            finding,
            RecoveryFinding::PendingApproval { call_id: id, .. } if *id == call_id
        )
    }));
}

#[test]
fn pending_tool_request_is_reported_with_a_conservative_policy() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let mut events =
        interrupted_tool_events(session_id, turn_id, call_id, "read", ExecutionId::new());
    events.pop();
    let report = classify_events(session_id, &events).unwrap();
    assert!(report.findings.iter().any(|finding| {
        matches!(
            finding,
            RecoveryFinding::PendingTool {
                call_id: id,
                policy: RecoveryPolicy::RetryRead,
                ..
            } if *id == call_id
        )
    }));
}

#[tokio::test]
async fn inspect_store_replays_events_and_reports_clean_after_completion() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let mut events =
        interrupted_tool_events(session_id, turn_id, call_id, "read", ExecutionId::new());
    events.push(event(
        session_id,
        Some(turn_id),
        7,
        EventPayload::ToolCompleted {
            call_id,
            result: ToolResult("ok".into()),
        },
    ));
    events.push(event(
        session_id,
        Some(turn_id),
        8,
        EventPayload::TurnCompleted {
            text: ModelText("done".into()),
        },
    ));
    let store = InMemoryEventStore::default();
    for event in events {
        store.append(event).await.unwrap();
    }
    let report = inspect_store(&store, session_id).await.unwrap();
    assert!(report.is_clean());
    assert_eq!(report.last_seq, EventSeq(8));
    assert!(report.state.active_turn.is_none());
}

#[tokio::test]
async fn recovery_is_idempotent_after_completion_marker() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let events = interrupted_tool_events(session_id, turn_id, call_id, "bash", ExecutionId::new());
    let store = InMemoryEventStore::default();
    for event in events {
        store.append(event).await.unwrap();
    }

    let first = recover_store(&store, session_id).await.unwrap();
    let first_last_seq = store.last_seq().await.unwrap();
    let second = recover_store(&store, session_id).await.unwrap();

    assert_eq!(first, second);
    assert_eq!(store.last_seq().await.unwrap(), first_last_seq);
}

#[tokio::test]
async fn recovery_resume_does_not_duplicate_partial_facts() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let events = interrupted_tool_events(session_id, turn_id, call_id, "bash", ExecutionId::new());
    let store = InMemoryEventStore::default();
    for event in events {
        store.append(event).await.unwrap();
    }
    store
        .append(event(
            session_id,
            None,
            0,
            EventPayload::RecoveryStarted {
                reason: "recovery interrupted".into(),
            },
        ))
        .await
        .unwrap();
    store
        .append(event(
            session_id,
            Some(turn_id),
            0,
            EventPayload::ToolOutcomeUnknown {
                call_id,
                reason: "result append was interrupted".into(),
            },
        ))
        .await
        .unwrap();

    recover_store(&store, session_id).await.unwrap();
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RecoveryStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ToolOutcomeUnknown { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RecoveryActionRequired { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RecoveryCompleted { .. }))
            .count(),
        1
    );
}

#[test]
fn policy_defaults_to_manual_decision_for_unknown_tools() {
    assert_eq!(
        policy_for_tool(&ToolName("read".into())),
        RecoveryPolicy::RetryRead
    );
    assert_eq!(
        policy_for_tool(&ToolName("edit".into())),
        RecoveryPolicy::InspectEditHash
    );
    assert_eq!(
        policy_for_tool(&ToolName("bash".into())),
        RecoveryPolicy::RequireUserDecision
    );
}

#[test]
fn approval_must_be_resolved_before_tool_starts() {
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let mut events =
        interrupted_tool_events(session_id, turn_id, call_id, "bash", ExecutionId::new());
    events.pop();
    events.push(event(
        session_id,
        Some(turn_id),
        6,
        EventPayload::ToolApprovalRequested { call_id },
    ));
    events.push(event(
        session_id,
        Some(turn_id),
        7,
        EventPayload::ToolStarted {
            call_id,
            execution_id: ExecutionId::new(),
        },
    ));
    let error = classify_events(session_id, &events).unwrap_err();
    assert!(
        matches!(error, HarnessError::InvariantViolation(message) if message.contains("before approval"))
    );
}
