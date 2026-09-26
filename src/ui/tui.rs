use super::{
    event_viewer::print_events,
    jsonl_client::{JsonlClient, WaitMode, WaitOutcome},
};
use crate::runtime::SessionId;
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Write as _, path::PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};

/// One interaction with the local input stream.
enum InputEvent {
    Line(String),
    Eof,
    Interrupt,
}

/// Reads one input line while staying responsive to Ctrl-C.
///
/// tokio's signal handler is installed process-wide the first time a
/// `ctrl_c()` future is created (for example by `wait_with_ctrl_c` during a
/// turn). Afterwards the default SIGINT disposition is gone, so every other
/// blocking read must also select on Ctrl-C or the UI would become
/// unkillable from the keyboard. Cancelling a partial `read_line` may drop
/// bytes the user had already typed on that line; that is acceptable for a
/// line-mode UI and is documented here instead of hidden.
async fn read_input_event(input: &mut BufReader<tokio::io::Stdin>) -> Result<InputEvent, String> {
    let mut line = String::new();
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    let bytes = tokio::select! {
        result = input.read_line(&mut line) => {
            result.map_err(|error| format!("read UI input: {error}"))?
        }
        _ = &mut signal => return Ok(InputEvent::Interrupt),
    };
    if bytes == 0 {
        return Ok(InputEvent::Eof);
    }
    Ok(InputEvent::Line(
        line.trim_end_matches(['\r', '\n']).to_owned(),
    ))
}

/// Configuration for the line-mode terminal client.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// None = fresh session in the durable layout (unique path per run).
    pub event_log: Option<PathBuf>,
    pub workspace: PathBuf,
    pub resume_session: Option<SessionId>,
    /// Provider/model resolved by the CLI (flags > config file > defaults).
    pub provider: String,
    pub model: Option<String>,
}

/// Entry point: full-screen mode on a terminal, line mode otherwise.
///
/// The line mode remains the pipe/CI-friendly fallback (and what integration
/// tests drive); the full-screen mode adds raw input, resize handling,
/// scrolling, and output folding (design §15, plan §13.3).
pub async fn run(options: TuiOptions) -> Result<(), String> {
    if super::terminal::stdin_is_tty() {
        return super::screen::run(options).await;
    }
    run_line_mode(options).await
}

/// Line-based UI: every message is one line, no terminal control.
async fn run_line_mode(options: TuiOptions) -> Result<(), String> {
    let mut client = JsonlClient::spawn().await?;
    let result = run_session(&mut client, options).await;
    if let Err(error) = result {
        client.abort().await;
        return Err(error);
    }
    client.finish().await
}

async fn run_session(client: &mut JsonlClient, options: TuiOptions) -> Result<(), String> {
    let negotiated = client
        .request("protocol.negotiate", json!({"version": 2}))
        .await?;
    let supports_events = negotiated
        .get("result")
        .and_then(|result| result.get("capabilities"))
        .and_then(|capabilities| capabilities.get("event_notifications"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !supports_events {
        return Err("JSONL server does not advertise event notifications".into());
    }

    let session_id = if let Some(session_id) = options.resume_session {
        let response = client
            .request(
                "session.resume",
                json!({
                    "event_log": options.event_log.clone().unwrap_or_else(|| {
                        std::env::temp_dir().join(format!(
                            "mini-harness-tui-{}.jsonl",
                            std::process::id()
                        ))
                    }),
                    "workspace": options.workspace,
                    "session_id": session_id,
                    "replay_events": true
                }),
            )
            .await?;
        client.append_response_events(&response);
        response_string(&response, "session_id")?
    } else {
        let mut params = json!({
            "event_log": options.event_log,
            "workspace": options.workspace,
            "provider": options.provider,
        });
        if let Some(model) = &options.model {
            params["model"] = json!(model);
        }
        let response = client.request("session.create", params).await?;
        response_string(&response, "session_id")?
    };
    let mut history = Vec::new();
    render_new_events(client, &mut history);
    println!("session {session_id}; enter a prompt, /reload, or /quit");

    let stdin = tokio::io::stdin();
    let mut input = BufReader::new(stdin);
    if let Some((pending_turn, call_id)) = pending_approval(&history) {
        if respond_to_approval(client, &mut input, &session_id, &pending_turn, &call_id).await? {
            run_turn(client, &mut input, &session_id, &pending_turn, &mut history).await?;
        }
        render_new_events(client, &mut history);
    }
    loop {
        print!("> ");
        std::io::stdout()
            .flush()
            .map_err(|error| format!("flush prompt: {error}"))?;
        let line = match read_input_event(&mut input).await? {
            InputEvent::Eof => break,
            InputEvent::Interrupt => {
                // Exit instead of being swallowed: once tokio owns SIGINT,
                // pressing Ctrl-C at the prompt must still close the UI.
                println!("^C");
                break;
            }
            InputEvent::Line(line) => line,
        };
        match line.as_str() {
            "/quit" => break,
            "/reload" | "r" => {
                print_events(history.clone());
                continue;
            }
            "" => continue,
            prompt => {
                let started = client
                    .request(
                        "turn.start",
                        json!({"session_id": session_id, "prompt": prompt}),
                    )
                    .await?;
                let turn_id = response_string(&started, "turn_id")?;
                run_turn(client, &mut input, &session_id, &turn_id, &mut history).await?;
            }
        }
        render_new_events(client, &mut history);
    }

    client.request("session.shutdown", json!({})).await?;
    Ok(())
}

async fn run_turn(
    client: &mut JsonlClient,
    input: &mut BufReader<tokio::io::Stdin>,
    session_id: &str,
    turn_id: &str,
    history: &mut Vec<Value>,
) -> Result<(), String> {
    loop {
        let (outcome, interrupted) = wait_with_ctrl_c(client, session_id, turn_id).await?;
        render_new_events(client, history);
        if interrupted && matches!(&outcome, WaitOutcome::Pending) {
            let response = client
                .request(
                    "turn.cancel",
                    json!({"session_id": session_id, "turn_id": turn_id}),
                )
                .await?;
            println!("cancel requested: {}", response["result"]);
            continue;
        }
        match outcome {
            WaitOutcome::Pending => continue,
            WaitOutcome::Cancelled(response) => {
                client.append_response_events(&response);
                println!("turn cancelled");
                render_new_events(client, history);
                return Ok(());
            }
            WaitOutcome::ApprovalPending => {
                let (pending_turn, call_id) = pending_approval(history)
                    .ok_or_else(|| "approval requested without an event projection".to_owned())?;
                if !respond_to_approval(client, input, session_id, &pending_turn, &call_id).await? {
                    render_new_events(client, history);
                    return Ok(());
                }
                continue;
            }
            WaitOutcome::Completed(response) => {
                client.append_response_events(&response);
                let text = response
                    .get("result")
                    .and_then(|result| result.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                println!("turn completed: {text}");
                render_new_events(client, history);
                return Ok(());
            }
        }
    }
}

async fn respond_to_approval(
    client: &mut JsonlClient,
    input: &mut BufReader<tokio::io::Stdin>,
    session_id: &str,
    turn_id: &str,
    call_id: &str,
) -> Result<bool, String> {
    let decision = read_approval(input, call_id).await?;
    let response = client
        .request(
            "approval.respond",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "call_id": call_id,
                "approved": decision
            }),
        )
        .await?;
    println!("approval persisted: {}", response["result"]);
    Ok(response["result"]["completed"] != Value::Bool(false))
}

async fn wait_with_ctrl_c(
    client: &mut JsonlClient,
    session_id: &str,
    turn_id: &str,
) -> Result<(WaitOutcome, bool), String> {
    let wait = client.wait_turn(session_id, turn_id, WaitMode::Poll);
    tokio::pin!(wait);
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    tokio::select! {
        outcome = &mut wait => Ok((outcome?, false)),
        result = &mut signal => {
            result.map_err(|error| format!("listen for Ctrl-C: {error}"))?;
            Ok((wait.await?, true))
        }
    }
}

async fn read_approval(
    input: &mut BufReader<tokio::io::Stdin>,
    call_id: &str,
) -> Result<bool, String> {
    loop {
        print!("approve tool call {call_id}? [y/N] ");
        std::io::stdout()
            .flush()
            .map_err(|error| format!("flush approval prompt: {error}"))?;
        match read_input_event(input).await? {
            // Both Ctrl-C and EOF take the safe default (deny), matching the
            // existing EOF behaviour; Ctrl-C never silently approves.
            InputEvent::Interrupt | InputEvent::Eof => return Ok(false),
            InputEvent::Line(line) => match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "" | "n" | "no" => return Ok(false),
                _ => println!("please answer y or n"),
            },
        }
    }
}

fn pending_approval(history: &[Value]) -> Option<(String, String)> {
    let mut pending = Vec::new();
    for value in history {
        let Some(params) = value.get("params") else {
            continue;
        };
        let Some(event) = params.get("event") else {
            continue;
        };
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(call_id) = event.get("tool_call_id").and_then(Value::as_str) else {
            continue;
        };
        match event_type {
            "tool.approval.requested" => {
                if let Some(turn_id) = params.get("turn_id").and_then(Value::as_str) {
                    pending.push((turn_id.to_owned(), call_id.to_owned()));
                }
            }
            "tool.approval.responded" => {
                pending.retain(|(_, pending_call_id)| pending_call_id != call_id);
            }
            _ => {}
        }
    }
    pending.pop()
}

fn render_new_events(client: &mut JsonlClient, history: &mut Vec<Value>) {
    let seen = history
        .iter()
        .filter_map(event_seq)
        .collect::<BTreeSet<_>>();
    let events = client
        .take_events()
        .into_iter()
        .filter(|event| event_seq(event).is_none_or(|seq| !seen.contains(&seq)))
        .collect::<Vec<_>>();
    if events.is_empty() {
        return;
    }
    history.extend(events.iter().cloned());
    print_events(events);
}

fn event_seq(value: &Value) -> Option<u64> {
    value
        .get("params")
        .and_then(|params| params.get("seq"))
        .and_then(Value::as_u64)
}

fn response_string(response: &Value, field: &str) -> Result<String, String> {
    response
        .get("result")
        .and_then(|result| result.get(field))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("JSONL response did not include {field}"))
}
