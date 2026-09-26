use mini_harness::{
    durable::{Event, EventPayload, reduce},
    error::ToolError,
    runtime::{
        EventSeq, ExecutionId, HistoryItem, ModelText, SessionId, SessionState, SessionStatus,
        ToolCallId, ToolName, TurnId, UserInput,
    },
};

fn event(session: SessionId, turn: Option<TurnId>, seq: u64, payload: EventPayload) -> Event {
    let mut event = Event::new(session, turn, payload);
    event.seq = EventSeq(seq);
    event
}

#[test]
fn hand_written_completion_without_model_response_preserves_final_text() {
    let session = SessionId::new();
    let turn = TurnId::new();
    let mut state = SessionState::new(session);
    let events = [
        event(session, None, 1, EventPayload::SessionCreated),
        event(
            session,
            None,
            2,
            EventPayload::UserInputRecorded {
                input: UserInput("hello".into()),
            },
        ),
        event(session, Some(turn), 3, EventPayload::TurnStarted),
        event(
            session,
            Some(turn),
            4,
            EventPayload::TurnCompleted {
                text: ModelText("done".into()),
            },
        ),
    ];
    for item in &events {
        reduce(&mut state, item).unwrap();
    }

    assert_eq!(
        state.history,
        vec![
            HistoryItem::User(UserInput("hello".into())),
            HistoryItem::Assistant(ModelText("done".into())),
        ]
    );
    assert_eq!(state.status, SessionStatus::Idle);
}

#[test]
fn failed_tool_cannot_be_followed_by_turn_completed() {
    let session = SessionId::new();
    let turn = TurnId::new();
    let call = ToolCallId::new();
    let mut state = SessionState::new(session);
    let preceding = [
        event(session, None, 1, EventPayload::SessionCreated),
        event(
            session,
            None,
            2,
            EventPayload::UserInputRecorded {
                input: UserInput("use the tool".into()),
            },
        ),
        event(session, Some(turn), 3, EventPayload::TurnStarted),
        event(
            session,
            Some(turn),
            4,
            EventPayload::ToolRequested {
                call_id: call,
                name: ToolName("read".into()),
                input: serde_json::json!({"path": "missing.txt"}),
            },
        ),
        event(
            session,
            Some(turn),
            5,
            EventPayload::ToolStarted {
                call_id: call,
                execution_id: ExecutionId::new(),
            },
        ),
        event(
            session,
            Some(turn),
            6,
            EventPayload::ToolFailed {
                call_id: call,
                error: ToolError::from("file missing"),
            },
        ),
    ];
    for item in &preceding {
        reduce(&mut state, item).unwrap();
    }
    let before = state.clone();
    let invalid = event(
        session,
        Some(turn),
        7,
        EventPayload::TurnCompleted {
            text: ModelText("done".into()),
        },
    );
    assert!(reduce(&mut state, &invalid).is_err());
    assert_eq!(state, before);

    let failed = event(
        session,
        Some(turn),
        7,
        EventPayload::TurnFailed {
            error: "tool failed".into(),
        },
    );
    reduce(&mut state, &failed).unwrap();
    assert_eq!(state.status, SessionStatus::Failed);
    assert!(state.active_turn.is_none());
}
