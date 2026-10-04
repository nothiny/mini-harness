use super::super::provider::{ModelRequest, ModelUsage, ToolCall};
use super::OpenAiProviderConfig;
use crate::{
    error::ProviderError,
    runtime::{HistoryItem, ProviderContinuation, ToolCallId, ToolName},
};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, time::Duration};

pub(super) struct ParsedResponse {
    pub(super) response_id: String,
    pub(super) text: String,
    pub(super) calls: Vec<ParsedToolCall>,
    pub(super) usage: Option<ModelUsage>,
    /// Chain-of-thought from thinking models (DeepSeek flash/reasoner).
    /// Must be replayed in the next request's assistant message.
    pub(super) reasoning_content: Option<String>,
}

pub(super) struct ParsedToolCall {
    pub(super) call: ToolCall,
    pub(super) native_call_id: String,
}

pub(super) fn build_request_body(
    config: &OpenAiProviderConfig,
    request: &ModelRequest,
) -> Result<Value, ProviderError> {
    // A per-request model override (design §6.1) takes precedence over the
    // provider's configured default; continuation matching uses the same
    // effective model so a mid-session switch deliberately falls back to
    // normalized history replay instead of reusing a stale response id.
    let effective_model = request
        .model
        .as_ref()
        .map(|name| name.0.as_str())
        .unwrap_or(config.model.as_str());
    let continuation = request.continuation.as_ref();
    let continuation_matches_config = continuation.is_some_and(|continuation| {
        continuation.provider == "openai"
            && continuation.model == effective_model
            && continuation.endpoint == config.endpoint
            && continuation.response_id.is_some()
    });
    // Response IDs are scoped to the provider configuration that created
    // them.  A resumed session may intentionally switch model or endpoint;
    // in that case replay normalized history instead of rejecting the turn or
    // sending a stale `previous_response_id`.
    let input = if continuation_matches_config {
        build_input(request.history.as_slice(), continuation)?
    } else {
        build_replay_input(request.history.as_slice())
    };
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name.0,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        })
        .collect::<Vec<_>>();
    let mut body = Map::new();
    body.insert("model".into(), Value::String(effective_model.to_owned()));
    body.insert("input".into(), Value::Array(input));
    body.insert("tools".into(), Value::Array(tools));
    body.insert("store".into(), Value::Bool(true));
    body.insert("stream".into(), Value::Bool(config.stream));
    body.insert("parallel_tool_calls".into(), Value::Bool(false));
    if let Some(instructions) = &request.system_instructions {
        body.insert("instructions".into(), Value::String(instructions.clone()));
    }
    if continuation_matches_config {
        if let Some(response_id) =
            continuation.and_then(|continuation| continuation.response_id.clone())
        {
            body.insert("previous_response_id".into(), Value::String(response_id));
        }
    }
    Ok(Value::Object(body))
}

/// Replays the normalized conversation when a persisted Responses response
/// cannot be reused (for example after switching model or endpoint).  Tool
/// outputs are represented as user content because the new endpoint does not
/// know the old response's native function-call IDs.
fn build_replay_input(history: &[HistoryItem]) -> Vec<Value> {
    history
        .iter()
        .map(|item| match item {
            HistoryItem::User(input) => json!({"role": "user", "content": input.0}),
            HistoryItem::Assistant(text) => json!({"role": "assistant", "content": text.0}),
            HistoryItem::Tool { name, result, .. } => json!({
                "role": "user",
                "content": format!("Tool `{}` result:\n{}", name.0, result.0),
            }),
        })
        .collect()
}

fn build_input(
    history: &[HistoryItem],
    continuation: Option<&ProviderContinuation>,
) -> Result<Vec<Value>, ProviderError> {
    let start = continuation.map_or(0, |continuation| continuation.history_cursor);
    if start > history.len() {
        return Err(ProviderError::from("invalid continuation history cursor"));
    }
    history[start..]
        .iter()
        .filter_map(|item| match item {
            HistoryItem::User(input) => Some(Ok(json!({"role": "user", "content": input.0}))),
            HistoryItem::Assistant(text) => {
                if continuation.is_some() {
                    None
                } else {
                    Some(Ok(json!({"role": "assistant", "content": text.0})))
                }
            }
            HistoryItem::Tool {
                call_id, result, ..
            } => Some(
                continuation
                    .and_then(|continuation| continuation.native_call_ids.get(call_id))
                    .map(|native_call_id| {
                        Ok(json!({
                            "type": "function_call_output",
                            "call_id": native_call_id,
                            "output": result.0,
                        }))
                    })
                    .unwrap_or_else(|| {
                        Err(ProviderError::from("missing OpenAI native tool call id"))
                    }),
            ),
        })
        .collect()
}

pub(super) fn make_continuation(
    request: &ModelRequest,
    config: &OpenAiProviderConfig,
    calls: &[ParsedToolCall],
    response_id: &str,
) -> ProviderContinuation {
    let mut native_call_ids = std::collections::HashMap::new();
    for call in calls {
        native_call_ids.insert(call.call.call_id, call.native_call_id.clone());
    }
    ProviderContinuation {
        provider: "openai".into(),
        model: request
            .model
            .as_ref()
            .map(|name| name.0.clone())
            .unwrap_or_else(|| config.model.clone()),
        endpoint: config.endpoint.clone(),
        response_id: Some(response_id.into()),
        reasoning_content: None, // Responses API doesn't use reasoning_content
        reasoning_history: Vec::new(),
        native_call_ids,
        history_cursor: request.history.len(),
    }
}

pub(super) fn parse_json_response(body: &str) -> Result<ParsedResponse, ProviderError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ProviderError::from("OpenAI returned an invalid JSON response"))?;
    parse_response_value(&value)
}

fn parse_response_value(value: &Value) -> Result<ParsedResponse, ProviderError> {
    let response_id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::from("OpenAI response is missing id"))?
        .to_owned();
    validate_id(&response_id)?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::from("OpenAI response is missing status"))?
        .to_owned();
    if status != "completed" {
        return Err(ProviderError::from("OpenAI response was not completed"));
    }
    let top_level_text = value
        .get("output_text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut native_ids = HashSet::new();
    {
        let output = value
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::from("OpenAI response is missing output"))?;
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for part in content {
                            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(value) = part.get("text").and_then(Value::as_str) {
                                    text.push_str(value);
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call = parse_tool_call(item)?;
                    if calls.len() >= 32 || !native_ids.insert(call.native_call_id.clone()) {
                        return Err(ProviderError::from(
                            "invalid or oversized OpenAI tool call batch",
                        ));
                    }
                    calls.push(call);
                }
                Some("refusal") => {
                    return Err(ProviderError::from("OpenAI response was refused"));
                }
                _ => {}
            }
        }
    }
    if text.is_empty() {
        text.push_str(top_level_text);
    }
    if text.trim().is_empty() && calls.is_empty() {
        return Err(ProviderError::from(
            "OpenAI response contained no text or tool calls",
        ));
    }
    Ok(ParsedResponse {
        response_id,
        text,
        calls,
        usage: parse_usage(value.get("usage")),
        reasoning_content: None, // Responses API handles reasoning differently
    })
}

fn parse_tool_call(item: &Value) -> Result<ParsedToolCall, ProviderError> {
    let native_call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::from("OpenAI function call is missing call_id"))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ProviderError::from("OpenAI function call is missing name"))?;
    let arguments: Value = match item.get("arguments") {
        Some(Value::String(arguments)) => serde_json::from_str(arguments)
            .map_err(|_| ProviderError::from("OpenAI function call has invalid arguments"))?,
        Some(_) => {
            return Err(ProviderError::from(
                "OpenAI function arguments must be a JSON string",
            ));
        }
        None => {
            return Err(ProviderError::from(
                "OpenAI function call is missing arguments",
            ));
        }
    };
    if !arguments.is_object() {
        return Err(ProviderError::from(
            "OpenAI function arguments must be an object",
        ));
    }
    validate_id(native_call_id)?;
    let call_id = ToolCallId::new();
    Ok(ParsedToolCall {
        call: ToolCall {
            call_id,
            name: ToolName(name.into()),
            input: arguments,
        },
        native_call_id: native_call_id.into(),
    })
}

pub(super) fn parse_stream_response(body: &str) -> Result<ParsedResponse, ProviderError> {
    let mut streamed_text = String::new();
    for frame in body.replace("\r\n", "\n").split("\n\n") {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(&data)
            .map_err(|_| ProviderError::from("OpenAI returned an invalid streaming event"))?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                let response = event
                    .get("response")
                    .ok_or_else(|| ProviderError::from("missing completed response"))?;
                let mut response_with_delta = response.clone();
                if !streamed_text.is_empty()
                    && response_with_delta
                        .get("output_text")
                        .and_then(Value::as_str)
                        .is_none()
                {
                    response_with_delta["output_text"] = Value::String(streamed_text.clone());
                }
                let parsed = parse_response_value(&response_with_delta)?;
                if parsed.text.trim().is_empty() && parsed.calls.is_empty() {
                    return Err(ProviderError::from(
                        "OpenAI stream contained no text or tool calls",
                    ));
                }
                return Ok(parsed);
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    streamed_text.push_str(delta);
                }
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                return Err(ProviderError::from("OpenAI streaming response failed"));
            }
            _ => {}
        }
    }
    Err(ProviderError::from(
        "OpenAI stream ended before response.completed",
    ))
}

fn parse_usage(value: Option<&Value>) -> Option<ModelUsage> {
    let value = value?.as_object()?;
    Some(ModelUsage {
        input_tokens: value.get("input_tokens").and_then(Value::as_u64),
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
    })
}

pub(super) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    const MAX_RETRY_AFTER: u64 = 30;
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .map(|seconds| Duration::from_secs(seconds.min(MAX_RETRY_AFTER)))
                .or_else(|| {
                    DateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
                        .ok()
                        .map(|date| {
                            let seconds =
                                (date.with_timezone(&Utc) - Utc::now()).num_seconds().max(0) as u64;
                            Duration::from_secs(seconds.min(MAX_RETRY_AFTER))
                        })
                })
        })
}

pub(super) fn retryable_status(status: StatusCode, body: &str) -> bool {
    if !matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
    ) && !status.is_server_error()
    {
        return false;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let lower = body.to_ascii_lowercase();
        return !lower.contains("credit_balance_exhausted")
            && !lower.contains("spend_limit")
            && !lower.contains("usage_limit")
            && !lower.contains("insufficient_quota");
    }
    true
}

pub(super) fn is_local_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

fn validate_id(id: &str) -> Result<(), ProviderError> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(ProviderError::from("invalid OpenAI identifier"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Chat Completions wire format (OpenAI-compatible servers: DeepSeek, Kimi,
// Qwen, vLLM, Ollama, ...). Kept next to the Responses code so both styles
// share validation, retries, and redaction; the provider dispatches on
// `WireStyle`.
// ---------------------------------------------------------------------------

/// Builds chat-completions `messages` from normalized history.
///
/// The runtime's history stores an assistant summary text ("tool call: x")
/// before each tool result, not the raw tool_calls. Chat completions requires
/// every `role:tool` message to be preceded by an assistant message carrying
/// the matching `tool_calls`, so the builder synthesizes that assistant
/// message from the summary text (content) plus the tool identity and original
/// arguments retained in history. Only legacy snapshots without arguments
/// fall back to `{}`.
pub(super) fn build_cc_messages(
    history: &[HistoryItem],
    native_call_ids: &std::collections::HashMap<ToolCallId, String>,
    reasoning_history: &[Option<String>],
) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_assistant: Option<String> = None;
    let mut last_assistant: Option<usize> = None;
    let mut index = 0;
    while index < history.len() {
        match &history[index] {
            HistoryItem::User(input) => {
                flush_assistant(&mut messages, pending_assistant.take(), &mut last_assistant);
                messages.push(json!({"role": "user", "content": input.0}));
                index += 1;
            }
            HistoryItem::Assistant(text) => {
                flush_assistant(
                    &mut messages,
                    pending_assistant.replace(text.0.clone()),
                    &mut last_assistant,
                );
                index += 1;
            }
            HistoryItem::Tool { .. } => {
                // A single model response can contain multiple tool calls.
                // The normalized history stores one Tool item per result, so
                // gather the contiguous run back into one assistant message
                // with a matching `tool_calls` array.  Emitting one assistant
                // message per result loses the response boundary and, for
                // thinking models, leaves all but one message without the
                // required reasoning_content.
                let content = pending_assistant
                    .take()
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                let mut tool_calls = Vec::new();
                let mut tool_results = Vec::new();
                while index < history.len() {
                    let HistoryItem::Tool {
                        call_id,
                        name,
                        input,
                        result,
                    } = &history[index]
                    else {
                        break;
                    };
                    // Prefer the provider-native call id the model actually
                    // emitted; fall back to the runtime id when the
                    // continuation does not know it (fresh sessions,
                    // restored logs).
                    let native = native_call_ids
                        .get(call_id)
                        .cloned()
                        .unwrap_or_else(|| call_id.to_string());
                    tool_calls.push(json!({
                        "id": native,
                        "type": "function",
                        "function": {
                            "name": name.0,
                            "arguments": if input.is_null() {
                                "{}".to_owned()
                            } else {
                                input.to_string()
                            },
                        },
                    }));
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": native,
                        "content": result.0,
                    }));
                    index += 1;
                }
                messages.push(json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls,
                }));
                last_assistant = Some(messages.len() - 1);
                messages.extend(tool_results);
            }
        }
    }
    flush_assistant(&mut messages, pending_assistant, &mut last_assistant);

    // Thinking models (e.g. deepseek-flash) require reasoning_content on
    // every assistant reply that is replayed. Align the saved response list
    // from the right because ContextSnapshot may retain only a suffix of the
    // normalized history. Empty entries are intentional: they keep a later
    // response's reasoning paired with the correct assistant message.
    let assistant_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message["role"] == "assistant").then_some(index))
        .collect::<Vec<_>>();
    let assistant_offset = assistant_indices
        .len()
        .saturating_sub(reasoning_history.len());
    let reasoning_offset = reasoning_history
        .len()
        .saturating_sub(assistant_indices.len());
    for (index, reasoning) in assistant_indices
        .into_iter()
        .skip(assistant_offset)
        .zip(reasoning_history.iter().skip(reasoning_offset))
    {
        if let Some(reasoning) = reasoning {
            messages[index]["reasoning_content"] = json!(reasoning);
        }
    }
    messages
}

fn flush_assistant(
    messages: &mut Vec<Value>,
    text: Option<String>,
    last_assistant: &mut Option<usize>,
) {
    if let Some(text) = text {
        messages.push(json!({"role": "assistant", "content": text}));
        *last_assistant = Some(messages.len() - 1);
    }
}

pub(super) fn build_cc_request_body(
    config: &OpenAiProviderConfig,
    request: &ModelRequest,
) -> Result<Value, ProviderError> {
    let mut messages = Vec::new();
    if let Some(instructions) = &request.system_instructions {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    let continuation = request.continuation.as_ref();
    let native_call_ids = continuation
        .map(|c| c.native_call_ids.clone())
        .unwrap_or_default();
    let reasoning_history = continuation
        .map(|c| {
            if c.reasoning_history.is_empty() {
                c.reasoning_content
                    .as_ref()
                    .map(|reasoning| vec![Some(reasoning.clone())])
                    .unwrap_or_default()
            } else {
                c.reasoning_history.clone()
            }
        })
        .unwrap_or_default();
    messages.extend(build_cc_messages(
        &request.history,
        &native_call_ids,
        &reasoning_history,
    ));
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name.0,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect::<Vec<_>>();
    let mut body = Map::new();
    body.insert("model".into(), Value::String(config.model.clone()));
    body.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        body.insert("parallel_tool_calls".into(), Value::Bool(false));
    }
    body.insert("stream".into(), Value::Bool(config.stream));
    Ok(Value::Object(body))
}

pub(super) fn parse_cc_json_response(body: &str) -> Result<ParsedResponse, ProviderError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ProviderError::from("chat completions returned invalid JSON"))?;
    let response_id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::from("chat completion is missing id"))?
        .to_owned();
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::from("chat completion is missing choices"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::from("chat completion is missing message"))?;
    // Thinking models (deepseek-flash / deepseek-reasoner) return their
    // chain-of-thought in `reasoning_content`. It must be replayed in the
    // next request's assistant message, so we preserve it here.
    let reasoning_content = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut calls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for item in tool_calls {
            let native_call_id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::from("tool call is missing id"))?;
            if !seen.insert(native_call_id.to_owned()) || calls.len() >= 32 {
                return Err(ProviderError::from("invalid or oversized tool call batch"));
            }
            let function = item
                .get("function")
                .ok_or_else(|| ProviderError::from("tool call is missing function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| ProviderError::from("tool call is missing name"))?;
            // Chat Completions encodes function arguments as a JSON string.
            // Do not silently turn a missing/null/non-string value into `{}`:
            // doing so dispatches an invalid call to the tool (for example,
            // bash then reports that `command` is not a string) and makes the
            // approval UI show an opaque input.  Reject the provider response
            // at the protocol boundary instead, where the actual malformed
            // shape is still clear and no tool side effect can occur.
            let arguments = function
                .get("arguments")
                .ok_or_else(|| ProviderError::from("tool call is missing arguments"))?
                .as_str()
                .ok_or_else(|| ProviderError::from("tool call arguments must be a JSON string"))?;
            let input: Value = serde_json::from_str(arguments)
                .map_err(|_| ProviderError::from("tool call has invalid arguments JSON"))?;
            if !input.is_object() {
                return Err(ProviderError::from(
                    "tool call arguments must be a JSON object",
                ));
            }
            calls.push(ParsedToolCall {
                call: ToolCall {
                    call_id: ToolCallId::new(),
                    name: ToolName(name.into()),
                    input,
                },
                native_call_id: native_call_id.to_owned(),
            });
        }
    }
    if text.trim().is_empty() && calls.is_empty() {
        return Err(ProviderError::from(
            "chat completion contained no text or tool calls",
        ));
    }
    Ok(ParsedResponse {
        response_id,
        text,
        calls,
        usage: parse_cc_usage(value.get("usage")),
        reasoning_content,
    })
}

pub(super) fn parse_cc_stream_response(body: &str) -> Result<ParsedResponse, ProviderError> {
    let mut text = String::new();
    let mut reasoning_content = String::new();
    let mut tool_names: Vec<String> = Vec::new();
    let mut tool_arguments: Vec<String> = Vec::new();
    let mut tool_ids: Vec<Option<String>> = Vec::new();
    let mut response_id: Option<String> = None;
    for frame in body.replace("\r\n", "\n").split("\n\n") {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(&data)
            .map_err(|_| ProviderError::from("invalid streaming chat completion event"))?;
        if response_id.is_none() {
            if let Some(id) = event.get("id").and_then(Value::as_str) {
                response_id = Some(id.to_owned());
            }
        }
        let Some(delta) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            continue;
        };
        if let Some(chunk) = delta.get("content").and_then(Value::as_str) {
            text.push_str(chunk);
        }
        if let Some(chunk) = delta.get("reasoning_content").and_then(Value::as_str) {
            reasoning_content.push_str(chunk);
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for item in tool_calls {
                let index = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if tool_names.len() <= index {
                    tool_names.resize(index + 1, String::new());
                    tool_arguments.resize(index + 1, String::new());
                    tool_ids.resize(index + 1, None);
                }
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    tool_ids[index] = Some(id.to_owned());
                }
                if let Some(function) = item.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        tool_names[index].push_str(name);
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        tool_arguments[index].push_str(arguments);
                    }
                }
            }
        }
    }
    let mut calls = Vec::new();
    for ((name, arguments), id) in tool_names.into_iter().zip(tool_arguments).zip(tool_ids) {
        if name.is_empty() {
            continue;
        }
        let native_call_id = id.unwrap_or_default();
        if native_call_id.is_empty() {
            return Err(ProviderError::from("streamed tool call is missing id"));
        }
        let input: Value = serde_json::from_str(&arguments)
            .map_err(|_| ProviderError::from("streamed tool call has invalid arguments"))?;
        if !input.is_object() {
            return Err(ProviderError::from(
                "streamed tool call arguments must be a JSON object",
            ));
        }
        calls.push(ParsedToolCall {
            call: ToolCall {
                call_id: ToolCallId::new(),
                name: ToolName(name),
                input,
            },
            native_call_id,
        });
    }
    if text.trim().is_empty() && calls.is_empty() {
        return Err(ProviderError::from(
            "chat completions stream contained no text or tool calls",
        ));
    }
    Ok(ParsedResponse {
        response_id: response_id.unwrap_or_default(),
        text,
        calls,
        usage: None,
        reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
    })
}

fn parse_cc_usage(value: Option<&Value>) -> Option<ModelUsage> {
    let value = value?;
    let field = |name: &str| value.get(name).and_then(Value::as_u64);
    Some(ModelUsage {
        input_tokens: field("prompt_tokens"),
        output_tokens: field("completion_tokens"),
        total_tokens: field("total_tokens"),
    })
}

/// Chat completions has no server-side continuation: every request replays
/// the normalized history. The continuation object is still persisted so the
/// reducer's cursor bookkeeping stays uniform across providers.
pub(super) fn make_cc_continuation(
    request: &ModelRequest,
    config: &OpenAiProviderConfig,
    calls: &[ParsedToolCall],
    previous: Option<&ProviderContinuation>,
    reasoning_content: Option<&str>,
) -> ProviderContinuation {
    // Keep IDs from earlier rounds as well.  If a Chat Completions turn is
    // later cancelled or fails, its normalized history is replayed from the
    // beginning and older role=tool messages still need their provider-native
    // IDs.  Replacing this map with only the latest batch would make those
    // messages fall back to runtime UUIDs.
    let mut native_call_ids = previous
        .map(|continuation| continuation.native_call_ids.clone())
        .unwrap_or_default();
    native_call_ids.extend(
        calls
            .iter()
            .map(|call| (call.call.call_id, call.native_call_id.clone())),
    );
    const MAX_NATIVE_CALL_IDS: usize = 32;
    if native_call_ids.len() > MAX_NATIVE_CALL_IDS {
        let current_ids = calls
            .iter()
            .map(|call| call.call.call_id)
            .collect::<HashSet<_>>();
        while native_call_ids.len() > MAX_NATIVE_CALL_IDS {
            let Some(old_id) = native_call_ids
                .keys()
                .copied()
                .find(|id| !current_ids.contains(id))
            else {
                break;
            };
            native_call_ids.remove(&old_id);
        }
    }
    let mut reasoning_history = previous
        .map(|continuation| continuation.reasoning_history.clone())
        .unwrap_or_default();
    // Checkpoints created before reasoning_history existed still carry the
    // latest value in reasoning_content. Preserve it as the first known slot.
    if reasoning_history.is_empty() {
        if let Some(previous) =
            previous.and_then(|continuation| continuation.reasoning_content.as_ref())
        {
            reasoning_history.push(Some(previous.clone()));
        }
    }
    reasoning_history.push(reasoning_content.map(str::to_owned));
    const MAX_REASONING_HISTORY: usize = 64;
    if reasoning_history.len() > MAX_REASONING_HISTORY {
        let drop = reasoning_history.len() - MAX_REASONING_HISTORY;
        reasoning_history.drain(..drop);
    }
    ProviderContinuation {
        provider: "openai".into(),
        model: config.model.clone(),
        endpoint: config.endpoint.clone(),
        response_id: None,
        native_call_ids,
        history_cursor: request.history.len(),
        reasoning_content: reasoning_content.map(str::to_owned),
        reasoning_history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ModelText, ToolResult, UserInput};
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::collections::HashMap;

    #[test]
    fn rejects_completed_response_without_content() {
        let error = match parse_json_response(r#"{"id":"resp_1","status":"completed","output":[]}"#)
        {
            Ok(_) => panic!("empty response unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no text or tool calls"));
    }

    #[test]
    fn caps_retry_after_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("999999"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));
    }

    #[test]
    fn streaming_text_delta_is_used_when_completed_payload_omits_text() {
        let response = parse_stream_response(&(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n"
                .to_owned()
                + "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n\n"
        ))
        .unwrap();
        assert_eq!(response.text, "hello");
    }

    #[test]
    fn cc_replays_reasoning_on_the_last_assistant_reply() {
        let history = vec![
            HistoryItem::User(UserInput("hi".into())),
            HistoryItem::Assistant(ModelText("hello".into())),
        ];
        let messages = build_cc_messages(&history, &HashMap::new(), &[Some("thinking".into())]);
        assert_eq!(messages.last().unwrap()["reasoning_content"], "thinking");
    }

    #[test]
    fn cc_replays_reasoning_on_a_trailing_tool_call() {
        let history = vec![
            HistoryItem::User(UserInput("hi".into())),
            HistoryItem::Assistant(ModelText("tool call: read".into())),
            HistoryItem::Tool {
                call_id: ToolCallId::new(),
                name: ToolName("read".into()),
                input: json!({"path": "a.txt"}),
                result: ToolResult("x".into()),
            },
        ];
        let messages = build_cc_messages(&history, &HashMap::new(), &[Some("thinking".into())]);
        let assistant = messages
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .expect("synthesized tool-call assistant message");
        assert_eq!(assistant["reasoning_content"], "thinking");
    }

    #[test]
    fn cc_replays_each_reasoning_value_for_replayed_assistant_messages() {
        let history = vec![
            HistoryItem::User(UserInput("read it".into())),
            HistoryItem::Assistant(ModelText("tool call: read".into())),
            HistoryItem::Tool {
                call_id: ToolCallId::new(),
                name: ToolName("read".into()),
                input: json!({"path": "a.txt"}),
                result: ToolResult("contents".into()),
            },
            HistoryItem::Assistant(ModelText("done".into())),
            HistoryItem::User(UserInput("continue".into())),
        ];
        let messages = build_cc_messages(
            &history,
            &HashMap::new(),
            &[
                Some("tool reasoning".into()),
                Some("final reasoning".into()),
            ],
        );
        let assistants = messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 2);
        assert_eq!(assistants[0]["reasoning_content"], "tool reasoning");
        assert_eq!(assistants[1]["reasoning_content"], "final reasoning");
    }

    #[test]
    fn cc_aligns_legacy_single_reasoning_value_to_the_latest_assistant() {
        let history = vec![
            HistoryItem::User(UserInput("first".into())),
            HistoryItem::Assistant(ModelText("first answer".into())),
            HistoryItem::User(UserInput("second".into())),
            HistoryItem::Assistant(ModelText("second answer".into())),
        ];
        let messages = build_cc_messages(
            &history,
            &HashMap::new(),
            &[Some("latest reasoning".into())],
        );
        let assistants = messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .collect::<Vec<_>>();
        assert!(assistants[0].get("reasoning_content").is_none());
        assert_eq!(assistants[1]["reasoning_content"], "latest reasoning");
    }

    #[test]
    fn cc_aligns_a_history_suffix_to_the_latest_reasoning_values() {
        let history = vec![
            HistoryItem::User(UserInput("latest".into())),
            HistoryItem::Assistant(ModelText("latest answer".into())),
        ];
        let messages = build_cc_messages(
            &history,
            &HashMap::new(),
            &[
                Some("old reasoning".into()),
                Some("latest reasoning".into()),
            ],
        );
        assert_eq!(messages[1]["reasoning_content"], "latest reasoning");
    }

    #[test]
    fn cc_groups_a_tool_batch_before_aligning_reasoning() {
        let first_call = ToolCallId::new();
        let second_call = ToolCallId::new();
        let history = vec![
            HistoryItem::User(UserInput("inspect both files".into())),
            HistoryItem::Assistant(ModelText("tool calls: read, read".into())),
            HistoryItem::Tool {
                call_id: first_call,
                name: ToolName("read".into()),
                input: json!({"path": "a.txt"}),
                result: ToolResult("first".into()),
            },
            HistoryItem::Tool {
                call_id: second_call,
                name: ToolName("read".into()),
                input: json!({"path": "a.txt"}),
                result: ToolResult("second".into()),
            },
        ];
        let messages =
            build_cc_messages(&history, &HashMap::new(), &[Some("batch reasoning".into())]);
        let assistants = messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(assistants[0]["reasoning_content"], "batch reasoning");
        let tool_messages = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect::<Vec<_>>();
        assert_eq!(tool_messages.len(), 2);
    }

    #[test]
    fn cc_continuation_retains_native_ids_from_earlier_rounds() {
        let old_call = ToolCallId::new();
        let new_call = ToolCallId::new();
        let previous = ProviderContinuation {
            provider: "openai".into(),
            response_id: None,
            model: "deepseek-flash".into(),
            endpoint: "https://api.deepseek.com/chat/completions".into(),
            native_call_ids: [(old_call, "call_old".into())].into_iter().collect(),
            history_cursor: 0,
            reasoning_content: Some("old reasoning".into()),
            reasoning_history: vec![Some("old reasoning".into())],
        };
        let request = ModelRequest::new(Vec::new(), Vec::new());
        let parsed = ParsedToolCall {
            call: ToolCall {
                call_id: new_call,
                name: ToolName("read".into()),
                input: json!({}),
            },
            native_call_id: "call_new".into(),
        };
        let continuation = make_cc_continuation(
            &request,
            &OpenAiProviderConfig::new("deepseek-flash", "sk-test"),
            &[parsed],
            Some(&previous),
            Some("new reasoning"),
        );
        assert_eq!(
            continuation.native_call_ids.get(&old_call).unwrap(),
            "call_old"
        );
        assert_eq!(
            continuation.native_call_ids.get(&new_call).unwrap(),
            "call_new"
        );
    }

    #[test]
    fn cc_stream_accumulates_reasoning_deltas() {
        let response = parse_cc_stream_response(
            &("data: {\"id\":\"cc_reasoning\",\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n"
                .to_owned()
                + "data: {\"id\":\"cc_reasoning\",\"choices\":[{\"delta\":{\"reasoning_content\":\" more\",\"content\":\"answer\"}}]}\n\n"
                + "data: [DONE]\n\n"),
        )
        .unwrap();
        assert_eq!(response.reasoning_content.as_deref(), Some("think more"));
        assert_eq!(response.text, "answer");
    }

    #[test]
    fn cc_rejects_missing_tool_arguments_instead_of_dispatching_empty_input() {
        let body = json!({
            "id": "cc_missing_args",
            "choices": [{"message": {"role": "assistant", "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash"}
            }]}}]
        });
        let error = match parse_cc_json_response(&body.to_string()) {
            Ok(_) => panic!("missing arguments unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("missing arguments"));
    }

    #[test]
    fn cc_rejects_non_object_tool_arguments_before_runtime_dispatch() {
        let body = json!({
            "id": "cc_array_args",
            "choices": [{"message": {"role": "assistant", "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": "[\"echo hi\"]"}
            }]}}]
        });
        let error = match parse_cc_json_response(&body.to_string()) {
            Ok(_) => panic!("array arguments unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("must be a JSON object"));
    }
}
