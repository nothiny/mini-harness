use std::process::Command;

use tokio::io::AsyncWriteExt;

use mini_harness::{
    durable::{Event, EventPayload, EventStore, JsonlEventStore},
    runtime::{
        EventSeq, ExecutionId, ModelText, SessionId, ToolCallId, ToolName, TurnId, UserInput,
    },
};

fn event(session_id: SessionId, turn_id: Option<TurnId>, payload: EventPayload) -> Event {
    Event::new(session_id, turn_id, payload)
}

#[tokio::test]
async fn recovery_commands_inspect_mark_unknown_and_abandon() {
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("events.jsonl");
    let store = JsonlEventStore::new(log_path.clone());
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    for payload in [
        EventPayload::SessionCreated,
        EventPayload::UserInputRecorded {
            input: UserInput("run command".into()),
        },
        EventPayload::TurnStarted,
        EventPayload::ModelResponseRecorded {
            text: ModelText("tool call: bash".into()),
        },
        EventPayload::ToolRequested {
            call_id,
            name: ToolName("bash".into()),
            input: serde_json::json!({"command": "true"}),
        },
        EventPayload::ToolStarted {
            call_id,
            execution_id: ExecutionId::new(),
        },
    ] {
        let event_turn = match &payload {
            EventPayload::SessionCreated | EventPayload::UserInputRecorded { .. } => None,
            _ => Some(turn_id),
        };
        store
            .append(event(session_id, event_turn, payload))
            .await
            .unwrap();
    }

    let binary = env!("CARGO_BIN_EXE_mini-harness");
    let inspect = Command::new(binary)
        .args(["inspect", log_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let inspect_text = String::from_utf8(inspect.stdout).unwrap();
    assert!(inspect_text.contains("UnknownTool"));
    assert!(inspect_text.contains("RequireUserDecision"));

    let recover = Command::new(binary)
        .args(["recover", log_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(recover.status.success());
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ToolOutcomeUnknown { call_id: id, .. } if id == call_id
    )));

    let turn_text = turn_id.to_string();
    let abandon = Command::new(binary)
        .args(["abandon-turn", log_path.to_str().unwrap(), &turn_text])
        .output()
        .unwrap();
    assert!(abandon.status.success());
    let abandon_text = String::from_utf8(abandon.stdout).unwrap();
    assert!(abandon_text.contains("\"active_turn\": null"));
    assert!(abandon_text.contains("\"status\": \"Failed\""));
}

#[tokio::test]
async fn repair_partial_tail_preserves_complete_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let store = JsonlEventStore::new(path.clone());
    let session_id = SessionId::new();
    store
        .append(event(session_id, None, EventPayload::SessionCreated))
        .await
        .unwrap();
    tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await
        .unwrap()
        .write_all(br#"{"schema_version":1"#)
        .await
        .unwrap();

    assert!(store.repair_partial_tail().await.unwrap());
    let events = store.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id, session_id);
}

#[test]
fn recovery_commands_are_exposed_by_clap_help() {
    let binary = env!("CARGO_BIN_EXE_mini-harness");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("recover"));
    assert!(help_text.contains("abandon-turn"));

    let recover_help = Command::new(binary)
        .args(["recover", "--help"])
        .output()
        .unwrap();
    assert!(recover_help.status.success());
    assert!(
        String::from_utf8(recover_help.stdout)
            .unwrap()
            .contains("<REFERENCE>")
    );

    let abandon_help = Command::new(binary)
        .args(["abandon-turn", "--help"])
        .output()
        .unwrap();
    assert!(abandon_help.status.success());
    let abandon_text = String::from_utf8(abandon_help.stdout).unwrap();
    assert!(abandon_text.contains("<REFERENCE>"));
    assert!(abandon_text.contains("<TURN_ID>"));
}

#[tokio::test]
async fn run_and_resume_use_the_event_log_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("events.jsonl");
    let binary = env!("CARGO_BIN_EXE_mini-harness");

    let run = Command::new(binary)
        .args(["run", "hello", "--event-log", log_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(run.status.success());
    let run_output: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    let session_id = run_output["session_id"].as_str().unwrap();
    let checkpoint_path = log_path.with_extension("checkpoint.json");
    assert!(checkpoint_path.exists());

    let resume = Command::new(binary)
        .args([
            "resume",
            session_id,
            "--event-log",
            log_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(resume.status.success());
    assert!(serde_json::from_slice::<serde_json::Value>(&resume.stdout).is_ok());
}
