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

fn read_response(output: &mut JsonReader, id: i64) -> serde_json::Value {
    loop {
        let value = read_line(output);
        if value.get("id") == Some(&serde_json::json!(id)) {
            return value;
        }
    }
}

fn shutdown(mut child: Child, mut input: ChildStdin, output: &mut JsonReader) {
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":99, "method":"session.shutdown", "params":{}}),
    );
    assert!(read_response(output, 99).get("result").is_some());
    drop(input);
    child.wait().expect("wait server");
}

#[test]
fn legacy_requests_keep_events_in_turn_wait_response() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let (child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({"id":1, "method":"session.create", "params":{"event_log":log, "workspace":directory.path()}}),
    );
    let created = read_response(&mut output, 1);
    let session_id = created["result"]["session_id"].clone();
    send(
        &mut input,
        serde_json::json!({"id":2, "method":"turn.start", "params":{"session_id":session_id, "input":"hello"}}),
    );
    let started = read_response(&mut output, 2);
    let turn_id = started["result"]["turn_id"].clone();
    send(
        &mut input,
        serde_json::json!({"id":3, "method":"turn.wait", "params":{"turn_id":turn_id}}),
    );
    let waited = read_response(&mut output, 3);
    let events = waited["result"]["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| {
        event.get("event_id").is_some()
            && event.get("timestamp").is_some()
            && event.get("schema_version").is_some()
            && event.get("payload").is_some()
    }));
    shutdown(child, input, &mut output);
}

#[test]
fn protocol_negotiate_reports_revision_and_message_limit() {
    let (mut child, mut input, mut output) = spawn_server();
    send(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"protocol.negotiate", "params":{"version":2}}),
    );
    let response = read_response(&mut output, 1);
    assert_eq!(response["result"]["protocol_version"], 2);
    assert_eq!(
        response["result"]["capabilities"]["max_message_bytes"],
        1_048_576
    );
    drop(input);
    child.wait().expect("wait server");
}
