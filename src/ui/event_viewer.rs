use super::jsonl_client::{JsonlClient, WaitMode, WaitOutcome};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_SUMMARY_VALUE_CHARS: usize = 256;

/// Options for the protocol-only event viewer.
///
/// The viewer deliberately has no runtime state of its own. It starts a
/// `serve --stdio` child, drives the public JSONL protocol, and renders the
/// event projections returned by that child.
#[derive(Debug, Clone)]
pub struct EventViewerOptions {
    pub event_log: std::path::PathBuf,
    pub workspace: std::path::PathBuf,
    pub prompt: String,
    /// Provider/model resolved by the CLI (flags > config file > defaults).
    pub provider: String,
    pub model: Option<String>,
}

/// Runs one turn through the JSONL server and prints a compact event stream.
pub async fn run(options: EventViewerOptions) -> Result<(), String> {
    if options.prompt.trim().is_empty() {
        return Err("usage: mini-harness events <prompt>".into());
    }
    let mut client = JsonlClient::spawn().await?;
    let result = drive_protocol(&mut client, &options).await;
    if let Err(error) = result {
        client.abort().await;
        return Err(error);
    }
    client.finish().await
}

async fn drive_protocol(
    client: &mut JsonlClient,
    options: &EventViewerOptions,
) -> Result<(), String> {
    let negotiated = client
        .request("protocol.negotiate", json!({"version": 2}))
        .await?;
    let capabilities = negotiated
        .get("result")
        .and_then(|result| result.get("capabilities"))
        .ok_or_else(|| "protocol.negotiate response did not include capabilities".to_owned())?;
    if capabilities
        .get("event_notifications")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("JSONL server does not advertise event notifications".into());
    }

    let mut params = json!({
        "event_log": options.event_log,
        "workspace": options.workspace,
        "provider": options.provider,
    });
    if let Some(model) = &options.model {
        params["model"] = json!(model);
    }
    let created = client.request("session.create", params).await?;
    let session_id = response_string(&created, "session_id")?;
    let started = client
        .request(
            "turn.start",
            json!({"session_id": session_id, "prompt": options.prompt}),
        )
        .await?;
    let turn_id = response_string(&started, "turn_id")?;
    let waited = match client
        .wait_turn(&session_id, &turn_id, WaitMode::Blocking)
        .await?
    {
        WaitOutcome::Completed(response) => response,
        WaitOutcome::Pending => return Err("turn is still running".into()),
        WaitOutcome::ApprovalPending => {
            return Err("event viewer cannot complete an approval-pending turn".into());
        }
        WaitOutcome::Cancelled(_) => return Err("turn was cancelled".into()),
    };
    if waited
        .get("result")
        .and_then(|result| result.get("completed"))
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("turn.wait did not report completed=true".into());
    }
    client.append_response_events(&waited);
    print_events(client.take_events());
    client.request("session.shutdown", json!({})).await?;
    Ok(())
}

fn response_string(response: &Value, field: &str) -> Result<String, String> {
    response
        .get("result")
        .and_then(|result| result.get(field))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("JSONL response did not include {field}"))
}

pub(crate) fn print_events(events: Vec<Value>) {
    let mut by_seq = BTreeMap::new();
    for value in events {
        let Some(params) = value.get("params") else {
            continue;
        };
        let Some(seq) = params.get("seq").and_then(Value::as_u64) else {
            continue;
        };
        by_seq.entry(seq).or_insert_with(|| value);
    }
    for (seq, value) in by_seq {
        let event = &value["params"]["event"];
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let summary = event_summary(event);
        if summary.is_empty() {
            println!("event seq={seq} type={event_type}");
        } else {
            println!("event seq={seq} type={event_type} {summary}");
        }
    }
}

fn event_summary(event: &Value) -> String {
    let mut fields = Vec::new();
    for (key, label) in [
        ("tool", "tool"),
        ("tool_call_id", "tool_call_id"),
        ("execution_id", "execution_id"),
        ("approved", "approved"),
        ("outcome", "outcome"),
        ("reason", "reason"),
    ] {
        if let Some(value) = event.get(key) {
            fields.push(format!("{label}={}", compact_value(value)));
        }
    }
    fields.join(" ")
}

fn compact_value(value: &Value) -> String {
    let raw = match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    };
    let mut compact = raw
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_SUMMARY_VALUE_CHARS)
        .collect::<String>();
    if raw.chars().count() > MAX_SUMMARY_VALUE_CHARS {
        compact.push('…');
    }
    compact
}
