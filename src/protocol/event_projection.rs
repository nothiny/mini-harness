use crate::durable::{Event, EventPayload};
use serde_json::{Value, json};

pub(super) fn project_event(event: &Event, include_legacy_payload: bool) -> Value {
    let mut projected = json!({"type": event_type(&event.payload)});
    let object = projected
        .as_object_mut()
        .expect("event projection always starts as an object");
    // Keep the durable identity fields on the notification projection. New
    // clients can use the compact `type` fields below, while clients written
    // against the original event envelope can continue to correlate events
    // by ID, timestamp, sequence and schema version.
    object.insert("event_id".into(), json!(event.event_id));
    object.insert("timestamp".into(), json!(event.timestamp));
    object.insert("schema_version".into(), json!(event.schema_version));
    if include_legacy_payload {
        object.insert("payload".into(), json!(event.payload));
    }
    match &event.payload {
        EventPayload::ToolRequested {
            call_id,
            name,
            input,
        } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("tool".into(), json!(name.0));
            // A safe, truncated summary of the tool input so clients can
            // show what a pending approval would actually do. The full
            // input stays in the durable log only.
            object.insert("input_summary".into(), json!(summarize_tool_input(input)));
        }
        EventPayload::ToolStarted {
            call_id,
            execution_id,
        } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("execution_id".into(), json!(execution_id));
        }
        EventPayload::ToolApprovalRequested { call_id }
        | EventPayload::ToolCompleted { call_id, .. }
        | EventPayload::ToolFailed { call_id, .. }
        | EventPayload::ToolOutcomeUnknown { call_id, .. } => {
            object.insert("tool_call_id".into(), json!(call_id));
        }
        EventPayload::ToolBatchDeferred { call_ids, .. } => {
            object.insert("tool_call_ids".into(), json!(call_ids));
        }
        EventPayload::ToolApprovalResponded { call_id, approved } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("approved".into(), json!(approved));
        }
        EventPayload::ToolPolicyDenied { call_id, reason } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("reason".into(), json!(reason));
        }
        EventPayload::ProviderAttemptRecorded {
            attempt,
            client_request_id,
            request_id,
            response_id,
            outcome,
            ..
        } => {
            object.insert("attempt".into(), json!(attempt));
            object.insert("client_request_id".into(), json!(client_request_id));
            object.insert("request_id".into(), json!(request_id));
            object.insert("response_id".into(), json!(response_id));
            object.insert("outcome".into(), json!(outcome));
        }
        _ => {}
    }
    json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": {
            "seq": event.seq,
            "session_id": event.session_id,
            "turn_id": event.turn_id,
            "event": projected,
        }
    })
}

pub(super) fn event_type(payload: &EventPayload) -> &'static str {
    payload.kind()
}

/// Extracts a human-readable one-liner from a tool's input for the v2
/// event projection. Shows the field a user needs to make an approval
/// decision (the command for bash, the path for read/edit), truncated.
fn summarize_tool_input(input: &serde_json::Value) -> String {
    const MAX_SUMMARY_CHARS: usize = 100;
    let summary = if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
        command
    } else if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        path
    } else if input.get("old_text").is_some() {
        "(text edit)"
    } else {
        "(opaque input)"
    };
    let truncated: String = summary.chars().take(MAX_SUMMARY_CHARS).collect();
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{EventId, EventSeq, SessionId, ToolCallId};

    #[test]
    fn policy_denial_projection_keeps_the_reason_and_id() {
        let call_id = ToolCallId::new();
        let event = Event {
            schema_version: Event::CURRENT_SCHEMA_VERSION,
            event_id: EventId::new(),
            seq: EventSeq(7),
            timestamp: chrono::Utc::now(),
            session_id: SessionId::new(),
            turn_id: None,
            payload: EventPayload::ToolPolicyDenied {
                call_id,
                reason: "policy denied tool `bash`".into(),
            },
        };
        let projected = project_event(&event, false);
        assert_eq!(projected["params"]["event"]["type"], "tool.policy_denied");
        assert_eq!(
            projected["params"]["event"]["tool_call_id"],
            serde_json::json!(call_id)
        );
        assert_eq!(
            projected["params"]["event"]["reason"],
            "policy denied tool `bash`"
        );
        // The safe projection never carries the durable payload in v2.
        assert!(projected["params"]["event"].get("payload").is_none());
    }
}
