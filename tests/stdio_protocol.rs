use mini_harness::{
    durable::{Event, EventPayload, EventStore, JsonlEventStore},
    runtime::{EventSeq, SessionId, ToolCallId, ToolName, TurnId, UserInput},
};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use tempfile::tempdir;

type JsonReader = BufReader<ChildStdout>;

fn spawn_server() -> (Child, ChildStdin, JsonReader) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn stdio server");
    let input = child.stdin.take().expect("server stdin");
    let output = BufReader::new(child.stdout.take().expect("server stdout"));
    (child, input, output)
}

fn send(input: &mut ChildStdin, value: serde_json::Value) {
    serde_json::to_writer(&mut *input, &value).expect("encode request");
    input.write_all(b"\n").expect("write request");
    input.flush().expect("flush request");
}

fn read_line(output: &mut JsonReader) -> serde_json::Value {
    let mut line = String::new();
    output.read_line(&mut line).expect("read response");
    assert!(!line.is_empty(), "server exited before replying");
    serde_json::from_str(&line).expect("decode JSONL response")
}

/// Reads notifications emitted before a response and returns the matching response.
fn read_response(output: &mut JsonReader, id: i64) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut events = Vec::new();
    loop {
        let value = read_line(output);
        if value.get("id") == Some(&serde_json::json!(id)) {
            return (value, events);
        }
        events.push(value);
    }
}

fn event_seq(value: &serde_json::Value) -> Option<u64> {
    value
        .get("params")
        .and_then(|params| params.get("seq"))
        .and_then(serde_json::Value::as_u64)
}

fn assert_success(response: &serde_json::Value) {
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response.get("error").is_none() || response["error"].is_null());
    assert!(response.get("result").is_some());
}

fn shutdown(mut child: Child, mut input: ChildStdin, output: &mut JsonReader, id: i64) {
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"session.shutdown", "params":{}}),
    );
    let (response, _) = read_response(output, id);
    assert_success(&response);
    drop(input);
    child.wait().expect("wait server");
}

#[test]
fn stdio_session_turn_wait_returns_sequenced_event_notifications() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let (child, mut input, mut output) = spawn_server();

    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"session.create",
            "params":{"event_log":log, "workspace":directory.path()}
        }),
    );
    let (created, create_events) = read_response(&mut output, 1);
    assert_success(&created);
    let session_id = created["result"]["session_id"].as_str().unwrap().to_owned();
    assert!(create_events.iter().all(|event| event["method"] == "event"));

    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":2, "method":"turn.start",
            "params":{"session_id":session_id, "input":"hello"}
        }),
    );
    let (started, start_events) = read_response(&mut output, 2);
    assert_success(&started);
    let turn_id = started["result"]["turn_id"].as_str().unwrap().to_owned();

    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":3, "method":"turn.wait",
            "params":{"turn_id":turn_id}
        }),
    );
    let (waited, wait_events) = read_response(&mut output, 3);
    assert_success(&waited);
    assert_eq!(waited["result"]["completed"], true);
    assert_eq!(waited["result"]["text"], "mock response");

    let mut events = create_events;
    events.extend(start_events);
    events.extend(wait_events);
    assert!(!events.is_empty());
    let sequences = events.iter().filter_map(event_seq).collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(events.iter().all(|event| {
        event["jsonrpc"] == "2.0"
            && event["method"] == "event"
            && event["params"]["session_id"] == session_id
    }));
    assert!(events.iter().all(|event| {
        let projected = &event["params"]["event"];
        projected.get("input").is_none()
            && projected.get("output").is_none()
            && projected.get("command").is_none()
    }));

    shutdown(child, input, &mut output, 4);
}

#[test]
fn stdio_resume_starts_after_last_event_without_replaying_history() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let (child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"session.create",
            "params":{"event_log":log, "workspace":directory.path()}
        }),
    );
    let (created, mut events) = read_response(&mut output, 1);
    let session_id = created["result"]["session_id"].as_str().unwrap().to_owned();
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":2, "method":"turn.start", "params":{"input":"one"}}),
    );
    let (started, start_events) = read_response(&mut output, 2);
    events.extend(start_events);
    let turn_id = started["result"]["turn_id"].as_str().unwrap().to_owned();
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":3, "method":"turn.wait", "params":{"turn_id":turn_id}}),
    );
    let (waited, wait_events) = read_response(&mut output, 3);
    assert_eq!(waited["result"]["completed"], true);
    events.extend(wait_events);
    let last_seq = events.iter().filter_map(event_seq).max().unwrap();
    shutdown(child, input, &mut output, 4);

    let (child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":10, "method":"session.resume",
            "params":{"event_log":log, "session_id":session_id, "workspace":directory.path()}
        }),
    );
    let (resumed, replayed_events) = read_response(&mut output, 10);
    assert_success(&resumed);
    assert!(replayed_events.is_empty(), "resume replayed old events");

    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":11, "method":"turn.start", "params":{"input":"two"}}),
    );
    let (started, _) = read_response(&mut output, 11);
    let turn_id = started["result"]["turn_id"].as_str().unwrap().to_owned();
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":12, "method":"turn.wait", "params":{"turn_id":turn_id}}),
    );
    let (_, new_events) = read_response(&mut output, 12);
    assert!(!new_events.is_empty());
    assert!(
        new_events
            .iter()
            .filter_map(event_seq)
            .all(|seq| seq > last_seq)
    );
    shutdown(child, input, &mut output, 13);
}

#[test]
fn stdio_create_persists_a_session_before_the_first_turn() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let (child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"session.create",
            "params":{"event_log":log, "workspace":directory.path()}
        }),
    );
    let (created, _) = read_response(&mut output, 1);
    assert_success(&created);
    let created_event = read_line(&mut output);
    assert_eq!(created_event["params"]["event"]["type"], "session.created");
    shutdown(child, input, &mut output, 2);

    let (child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":3, "method":"session.resume",
            "params":{"event_log":log, "session_id":created["result"]["session_id"], "workspace":directory.path()}
        }),
    );
    let (resumed, replayed_events) = read_response(&mut output, 3);
    assert_success(&resumed);
    assert!(replayed_events.is_empty());
    shutdown(child, input, &mut output, 4);
}

#[test]
fn stdio_cancel_approval_and_unknown_requests_return_structured_errors() {
    let (mut child, mut input, mut output) = spawn_server();

    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"turn.cancel", "params":{"turn_id":"turn_00000000-0000-0000-0000-000000000000"}}),
    );
    let (cancel, _) = read_response(&mut output, 1);
    assert_eq!(cancel["error"]["code"], "no_session");
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":2, "method":"approval.respond", "params":{"turn_id":"turn_00000000-0000-0000-0000-000000000000", "call_id":"call_00000000-0000-0000-0000-000000000000", "approved":true}}),
    );
    let (approval, _) = read_response(&mut output, 2);
    assert_eq!(approval["error"]["code"], "no_session");
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":3, "method":"missing", "params":{}}),
    );
    let (unknown, _) = read_response(&mut output, 3);
    assert_eq!(unknown["error"]["code"], "method_not_found");
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":4, "method":"session.shutdown", "params":{}}),
    );
    let (shutdown_response, _) = read_response(&mut output, 4);
    assert_success(&shutdown_response);
    drop(input);
    child.wait().unwrap();
}

#[test]
fn stdio_approval_response_persists_and_completes_the_pending_turn() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("pending.jsonl");
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let call_id = ToolCallId::new();
    let marker = directory.path().join("approved");
    let input = serde_json::json!({
        "command": format!("echo approved > '{}'", marker.to_string_lossy())
    });
    let store = JsonlEventStore::new(log.clone());
    let events = [
        EventPayload::SessionCreated,
        EventPayload::UserInputRecorded {
            input: UserInput("run command".into()),
        },
        EventPayload::TurnStarted,
        EventPayload::ToolRequested {
            call_id,
            name: ToolName("bash".into()),
            input,
        },
        EventPayload::ToolApprovalRequested { call_id },
    ];
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for (index, payload) in events.into_iter().enumerate() {
            let mut event = Event::new(
                session_id,
                if index < 2 { None } else { Some(turn_id) },
                payload,
            );
            event.seq = EventSeq((index + 1) as u64);
            store.append(event).await.unwrap();
        }
    });

    let (child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"session.resume",
            "params":{"event_log":log, "session_id":session_id, "workspace":directory.path()}
        }),
    );
    let resumed = read_response(&mut output, 1).0;
    assert_success(&resumed);
    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":2, "method":"approval.respond",
            "params":{"turn_id":turn_id, "call_id":call_id, "approved":true}
        }),
    );
    let (approval, _) = read_response(&mut output, 2);
    assert_success(&approval);
    assert_eq!(approval["result"]["persisted"], true);
    assert_eq!(approval["result"]["completed"], true);
    assert_eq!(approval["result"]["text"], "mock response");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "approved");

    send(
        &mut input,
        serde_json::json!({
            "jsonrpc":"2.0", "id":3, "method":"turn.wait",
            "params":{"turn_id":turn_id}
        }),
    );
    let (waited, _) = read_response(&mut output, 3);
    assert_success(&waited);
    assert_eq!(waited["result"]["completed"], true);
    shutdown(child, input, &mut output, 4);
}

#[test]
fn stdio_invalid_json_is_a_jsonrpc_error_and_server_survives() {
    let (mut child, mut input, mut output) = spawn_server();
    input.write_all(b"not json\n").unwrap();
    input.flush().unwrap();
    let error = read_line(&mut output);
    assert_eq!(error["jsonrpc"], "2.0");
    assert_eq!(error["id"], serde_json::Value::Null);
    assert_eq!(error["error"]["code"], "invalid_json");
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":5, "method":"session.shutdown", "params":{}}),
    );
    let (response, _) = read_response(&mut output, 5);
    assert_success(&response);
    drop(input);
    child.wait().unwrap();
}

#[test]
fn stdio_server_flushes_notifications_without_a_wait_request() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let (child, mut input, mut output) = spawn_server();

    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","protocol_version":2,"id":1,"method":"session.create","params":{
            "event_log": log, "workspace": directory.path(),
        }}),
    );
    let (response, _) = read_response(&mut output, 1);
    assert_success(&response);
    let session_id = response["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Submit a turn and never send `turn.wait`: the event pump must still
    // push version 2 notifications while the turn runs and after it ends.
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"turn.start","params":{
            "session_id": session_id, "prompt": "background notifications",
        }}),
    );
    let (response, _) = read_response(&mut output, 2);
    assert_success(&response);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut seen_turn_completed = false;
    while !seen_turn_completed && std::time::Instant::now() < deadline {
        let value = read_line(&mut output);
        if value.get("method").and_then(serde_json::Value::as_str) == Some("event")
            && value["params"]["event"]["type"] == "turn.completed"
        {
            seen_turn_completed = true;
        }
    }
    assert!(
        seen_turn_completed,
        "expected a turn.completed notification without turn.wait"
    );
    shutdown(child, input, &mut output, 99);
}

#[test]
fn stdio_server_writes_checkpoints_at_turn_boundaries() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let checkpoint = directory.path().join("events.checkpoint.json");
    let (child, mut input, mut output) = spawn_server();

    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","protocol_version":2,"id":1,"method":"session.create","params":{
            "event_log": &log, "workspace": directory.path(),
        }}),
    );
    let (response, _) = read_response(&mut output, 1);
    assert_success(&response);
    let session_id = response["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"turn.start","params":{
            "session_id": session_id, "prompt": "checkpoint me",
        }}),
    );
    let (response, _) = read_response(&mut output, 2);
    assert_success(&response);
    let turn_id = response["result"]["turn_id"].as_str().unwrap().to_owned();
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"turn.wait","params":{
            "session_id": session_id, "turn_id": turn_id,
        }}),
    );
    let (response, _) = read_response(&mut output, 3);
    assert_success(&response);

    // The pump checkpoints through the actor at the turn boundary; allow a
    // couple of poll cycles before asserting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !checkpoint.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(checkpoint.exists(), "server did not write a checkpoint");
    shutdown(child, input, &mut output, 99);
}
