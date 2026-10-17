//! 安全事件投影: 把持久化事件转换为可发给客户端的通知。
//!
//! 原则: 完整的工具输入/输出只留在事件日志里；客户端收到的是
//! 有界的、脱敏的摘要（`input_summary` / `result_summary`），
//! 足以做审批决策，不足以泄露敏感内容。

use mini_harness::durable::{Event, EventPayload, ToolOutcome};
use serde_json::{Value, json};

pub(crate) fn project_event(event: &Event) -> Value {
    let mut projected = json!({"type": event.payload.kind()});
    let object = projected
        .as_object_mut()
        .expect("event projection always starts as an object");
    object.insert("event_id".into(), json!(event.event_id));
    object.insert("timestamp".into(), json!(event.timestamp));
    object.insert("schema_version".into(), json!(event.schema_version));
    match &event.payload {
        EventPayload::ToolRequested {
            call_id,
            name,
            input,
            requires_approval,
        } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("tool".into(), json!(name.0));
            object.insert("requires_approval".into(), json!(requires_approval));
            let detail = summarize_tool_input(input);
            object.insert("input_summary".into(), json!(detail.summary));
            object.insert("input_summary_truncated".into(), json!(detail.truncated));
            object.insert("input_summary_redacted".into(), json!(detail.redacted));
            object.insert("input_detail".into(), detail.detail);
        }
        EventPayload::ToolApprovalResponded { call_id, approved } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("approved".into(), json!(approved));
        }
        EventPayload::ToolCompleted {
            call_id,
            outcome,
            duration_ms,
        } => {
            object.insert("tool_call_id".into(), json!(call_id));
            object.insert("duration_ms".into(), json!(duration_ms));
            match outcome {
                ToolOutcome::Succeeded { result } => {
                    object.insert("result_summary".into(), summarize_tool_result(&result.0));
                }
                ToolOutcome::Rejected { error } | ToolOutcome::Failed { error } => {
                    let safe = sanitize(&error.to_string(), 200);
                    object.insert(
                        "error_summary".into(),
                        json!({"text":safe.text,"truncated":safe.truncated,"redacted":safe.redacted}),
                    );
                }
            }
        }
        EventPayload::ToolOutcomeUnknown { call_id, reason } => {
            object.insert("tool_call_id".into(), json!(call_id));
            let safe = sanitize(reason, 200);
            object.insert("reason".into(), json!(safe.text));
            object.insert("reason_truncated".into(), json!(safe.truncated));
            object.insert("reason_redacted".into(), json!(safe.redacted));
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

const MAX_SUMMARY_CHARS: usize = 100;
const MAX_DETAIL_CHARS: usize = 240;

struct InputSummary {
    summary: String,
    truncated: bool,
    redacted: bool,
    detail: Value,
}

fn summarize_tool_input(input: &Value) -> InputSummary {
    if let Some(command) = input.get("command").and_then(Value::as_str) {
        let safe = sanitize(command, MAX_DETAIL_CHARS);
        return InputSummary {
            summary: truncate(&safe.text, MAX_SUMMARY_CHARS).0,
            truncated: safe.truncated || safe.text.chars().count() > MAX_SUMMARY_CHARS,
            redacted: safe.redacted,
            detail: json!({"kind":"bash", "command":safe.text, "truncated":safe.truncated, "redacted":safe.redacted}),
        };
    }
    if let Some(path) = input.get("path").and_then(Value::as_str) {
        let safe = sanitize(path, MAX_DETAIL_CHARS);
        let kind = if input.get("old_text").is_some() {
            "edit"
        } else {
            "read"
        };
        let mut detail = json!({"kind":kind, "path":safe.text, "truncated":safe.truncated, "redacted":safe.redacted});
        if kind == "edit" {
            for (field, label) in [("old_text", "old_preview"), ("new_text", "new_preview")] {
                if let Some(value) = input.get(field).and_then(Value::as_str) {
                    let preview = sanitize(value, MAX_DETAIL_CHARS);
                    detail[label] = json!({"text": preview.text, "truncated": preview.truncated, "redacted": preview.redacted, "chars": value.chars().count()});
                }
            }
        }
        return InputSummary {
            summary: truncate(&safe.text, MAX_SUMMARY_CHARS).0,
            truncated: safe.truncated || safe.text.chars().count() > MAX_SUMMARY_CHARS,
            redacted: safe.redacted,
            detail,
        };
    }
    let fields = input
        .as_object()
        .map(|object| object.keys().take(16).collect::<Vec<_>>())
        .unwrap_or_default();
    InputSummary {
        summary: "(opaque input)".into(),
        truncated: false,
        redacted: false,
        detail: json!({"kind":"opaque", "fields":fields}),
    }
}

struct SafeText {
    text: String,
    truncated: bool,
    redacted: bool,
}

fn sanitize(value: &str, limit: usize) -> SafeText {
    let mut output = String::new();
    for ch in value.chars() {
        if ch == '\n' {
            output.push_str("\\n");
            continue;
        }
        if ch == '\r' {
            output.push_str("\\r");
            continue;
        }
        if ch == '\t' {
            output.push_str("\\t");
            continue;
        }
        if ch.is_control() {
            output.push('\u{fffd}');
            continue;
        }
        output.push(ch);
    }
    // Redact common credentials without attempting to parse shell syntax.
    // This is deliberately conservative: displaying a placeholder is safer
    // than presenting a plausible but incomplete secret.
    let mut redacted_output = String::with_capacity(output.len());
    let mut redacted = false;
    let mut redact_next = false;
    for token in output.split_inclusive(char::is_whitespace) {
        let mut token_out = token.to_owned();
        let lower = token.to_ascii_lowercase();
        let secret_key = lower.contains("api_key=")
            || lower.contains("api_key:")
            || lower.contains("apikey=")
            || lower.contains("apikey:")
            || lower.contains("access_token=")
            || lower.contains("access_token:")
            || lower.contains("token=")
            || lower.contains("token:")
            || lower.contains("secret=")
            || lower.contains("secret:")
            || lower.contains("password=")
            || lower.contains("password:")
            || lower.contains("authorization:")
            || lower == "bearer"
            || (lower.starts_with("sk-") && lower.len() >= 16)
            || (lower.starts_with("ghp_") && lower.len() >= 16)
            || (lower.starts_with("xoxb-") && lower.len() >= 16)
            || (lower.starts_with("aiza") && lower.len() >= 20)
            || redact_next;
        if secret_key {
            if let Some(index) = token.find('=') {
                token_out = format!(
                    "{}<redacted>{}",
                    &token[..=index],
                    &token[index + 1..]
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect::<String>()
                );
            } else if let Some(index) = token.find(':') {
                token_out = format!("{} <redacted>", &token[..=index]);
            } else if lower == "bearer" || redact_next {
                token_out = "Bearer <redacted>".into();
            } else if lower.starts_with("sk-")
                || lower.starts_with("ghp_")
                || lower.starts_with("xoxb-")
                || lower.starts_with("aiza")
            {
                token_out = "<redacted>".into();
            }
            redacted = true;
        }
        redact_next = lower == "bearer"
            || (lower.ends_with(':')
                && (lower.contains("authorization")
                    || lower.contains("api_key")
                    || lower.contains("apikey")
                    || lower.contains("access_token")
                    || lower.contains("token")
                    || lower.contains("secret")
                    || lower.contains("password")));
        redacted_output.push_str(&token_out);
    }
    let (text, truncated) = truncate(&redacted_output, limit);
    SafeText {
        text,
        truncated,
        redacted,
    }
}

fn truncate(value: &str, limit: usize) -> (String, bool) {
    let truncated = value.chars().count() > limit;
    let take = if truncated {
        limit.saturating_sub(1)
    } else {
        limit
    };
    let mut text: String = value.chars().take(take).collect();
    if truncated {
        text.push('\u{2026}');
    }
    (text, truncated)
}

fn summarize_tool_result(result: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(result) else {
        return json!({"kind":"opaque", "bytes":result.len(), "truncated":false, "redacted":false});
    };
    if let Some(error) = value.get("error").and_then(Value::as_object) {
        let kind = error
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("tool_failed");
        let message = error.get("message").and_then(Value::as_str).map(|text| {
            let safe = sanitize(text, 160);
            json!({"text":safe.text,"truncated":safe.truncated,"redacted":safe.redacted})
        });
        let (truncated, redacted) = error
            .get("message")
            .and_then(Value::as_str)
            .map(|text| {
                let safe = sanitize(text, 160);
                (safe.truncated, safe.redacted)
            })
            .unwrap_or((false, false));
        return json!({"kind":"error", "error_kind":kind, "message":message, "truncated":truncated, "redacted":redacted});
    }
    if value.get("stdout").is_some() || value.get("stderr").is_some() {
        let output = |name: &str| {
            let value = value.get(name).and_then(Value::as_object);
            json!({"bytes":value.and_then(|v| v.get("original_bytes")).cloned().unwrap_or(Value::Null), "retained_bytes":value.and_then(|v| v.get("retained_bytes")).cloned().unwrap_or(Value::Null), "truncated":value.and_then(|v| v.get("truncated")).cloned().unwrap_or(json!(false))})
        };
        let stdout = output("stdout");
        let stderr = output("stderr");
        let truncated = stdout["truncated"].as_bool().unwrap_or(false)
            || stderr["truncated"].as_bool().unwrap_or(false);
        return json!({"kind":"process", "exit_code":value.get("exit_code"), "signal":value.get("signal"), "timed_out":value.get("timed_out").unwrap_or(&Value::Bool(false)), "stdout":stdout, "stderr":stderr, "truncated":truncated, "redacted":false});
    }
    if value.get("old_hash").is_some() && value.get("new_hash").is_some() {
        return json!({"kind":"edit", "replacements":value.get("replacements"), "old_hash":value.get("old_hash"), "new_hash":value.get("new_hash"), "truncated":false, "redacted":false});
    }
    if value.get("text").is_some() {
        return json!({"kind":"read", "original_bytes":value.get("original_bytes"), "retained_bytes":value.get("retained_bytes"), "truncated":value.get("truncated").unwrap_or(&Value::Bool(false)), "redacted":false, "start_line":value.get("start_line"), "end_line":value.get("end_line")});
    }
    json!({"kind":"structured", "fields":value.as_object().map(|object| object.keys().take(16).collect::<Vec<_>>()).unwrap_or_default(), "truncated":false, "redacted":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use mini_harness::runtime::{EventId, EventSeq, SessionId, ToolCallId};

    #[test]
    fn approval_request_projection_carries_the_flag_and_summary() {
        let call_id = ToolCallId::new();
        let event = Event {
            schema_version: Event::CURRENT_SCHEMA_VERSION,
            event_id: EventId::new(),
            seq: EventSeq(7),
            timestamp: chrono::Utc::now(),
            session_id: SessionId::new(),
            turn_id: None,
            payload: EventPayload::ToolRequested {
                call_id,
                name: mini_harness::runtime::ToolName("bash".into()),
                input: json!({"command": "echo hi"}),
                requires_approval: true,
            },
        };
        let projected = project_event(&event);
        assert_eq!(projected["params"]["event"]["type"], "tool.requested");
        assert_eq!(
            projected["params"]["event"]["requires_approval"],
            serde_json::json!(true)
        );
        assert!(projected["params"]["event"].get("payload").is_none());
    }

    #[test]
    fn tool_input_projection_is_bounded_and_redacts_credentials() {
        let command = format!(
            "OPENAI_API_KEY=secret123 curl -H 'Authorization: Bearer sk-abcdefghijklmnop' {}",
            "x".repeat(500)
        );
        let input = json!({"command": command});
        let summary = summarize_tool_input(&input);
        assert!(summary.truncated);
        assert!(summary.redacted);
        assert!(!summary.summary.contains("secret123"));
        assert!(!summary.detail.to_string().contains("sk-abcdefghijklmnop"));
        assert!(!summary.detail.to_string().contains("secret123"));
        assert!(summary.detail["command"].as_str().unwrap().chars().count() <= MAX_DETAIL_CHARS);
    }

    #[test]
    fn tool_input_projection_escapes_terminal_controls_and_shows_edit_shape() {
        let input =
            json!({"path":"src/main.rs", "old_text":"before\u{1b}[31m", "new_text":"after"});
        let summary = summarize_tool_input(&input);
        assert!(
            summary.detail["old_preview"]["text"]
                .as_str()
                .unwrap()
                .contains('\u{fffd}')
        );
        assert_eq!(summary.detail["kind"], "edit");
        assert_eq!(summary.detail["path"], "src/main.rs");
    }

    #[test]
    fn process_result_projection_contains_metadata_but_not_output() {
        let result = json!({
            "stdout":{"text":"TOP SECRET", "truncated":false, "original_bytes":10, "retained_bytes":10},
            "stderr":{"text":"", "truncated":false, "original_bytes":0, "retained_bytes":0},
            "exit_code":0, "signal":null, "timed_out":false
        });
        let summary = summarize_tool_result(&result.to_string());
        assert_eq!(summary["kind"], "process");
        assert!(summary.to_string().contains("bytes"));
        assert!(!summary.to_string().contains("TOP SECRET"));
    }
}
