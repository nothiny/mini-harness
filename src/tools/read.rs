use super::spec::{RiskClass, Tool, ToolContext, ToolSpec};
use crate::{
    executor::ReadFileRequest,
    runtime::types::{ByteLimit, ToolName},
};
use async_trait::async_trait;
use serde_json::{Value, json};

const MAX_LINE_COUNT: u64 = 200;

#[derive(Debug)]
struct ReadInput {
    path: String,
    start_line: Option<u64>,
    end_line: Option<u64>,
}

pub struct ReadTool {
    pub max_bytes: usize,
}

impl ReadTool {
    fn error(kind: &str, message: impl Into<String>) -> String {
        json!({
            "error": {
                "kind": kind,
                "message": message.into(),
            }
        })
        .to_string()
    }

    fn parse_input(input: Value) -> Result<ReadInput, String> {
        let object = input
            .as_object()
            .ok_or_else(|| Self::error("invalid_input", "input must be a JSON object"))?;
        let allowed = ["path", "start_line", "end_line"];
        if let Some(name) = object.keys().find(|name| !allowed.contains(&name.as_str())) {
            return Err(Self::error(
                "invalid_input",
                format!("unknown field `{name}`"),
            ));
        }
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Self::error("invalid_input", "path must be a string"))?
            .to_owned();
        if path.trim().is_empty() {
            return Err(Self::error("invalid_input", "path must not be empty"));
        }
        let parse_line = |name: &str| -> Result<Option<u64>, String> {
            let Some(value) = object.get(name) else {
                return Ok(None);
            };
            let line = value.as_u64().ok_or_else(|| {
                Self::error(
                    "invalid_input",
                    format!("{name} must be a positive integer"),
                )
            })?;
            if line == 0 {
                return Err(Self::error("invalid_input", "line numbers are one-based"));
            }
            Ok(Some(line))
        };
        let start_line = parse_line("start_line")?;
        let end_line = parse_line("end_line")?;
        if let (Some(start), Some(end)) = (start_line, end_line) {
            if end < start {
                return Err(Self::error(
                    "invalid_input",
                    "end_line must be greater than or equal to start_line",
                ));
            }
        }
        Ok(ReadInput {
            path,
            start_line,
            end_line,
        })
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName("read".into()),
            description: "Read a text file in the workspace".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            risk: RiskClass::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: ToolContext<'_>) -> Result<String, String> {
        let input = Self::parse_input(input)?;
        if ctx.cancel.is_cancelled() {
            return Err(Self::error("cancelled", "read cancelled before execution"));
        }
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                return Err(Self::error("cancelled", "read cancelled"));
            }
            result = ctx.executor.read_file(ReadFileRequest {
                path: input.path,
                max_bytes: ByteLimit(self.max_bytes.min(ctx.output_limit.0)),
            }) => result,
        };
        let result = result.map_err(|error| {
            let details = serde_json::to_value(&error)
                .unwrap_or_else(|_| json!({"message": error.to_string()}));
            json!({
                "error": {
                    "kind": "read_failed",
                    "message": error.to_string(),
                    "details": details,
                }
            })
            .to_string()
        })?;
        if ctx.cancel.is_cancelled() {
            return Err(Self::error("cancelled", "read cancelled"));
        }

        let lines: Vec<&str> = result.text.split_inclusive('\n').collect();
        let start = input.start_line.unwrap_or(1);
        let max_end = start.saturating_add(MAX_LINE_COUNT - 1);
        let requested_end = input.end_line.unwrap_or(max_end);
        let available_end = lines.len() as u64;
        let end = requested_end.min(max_end).min(available_end.max(start));
        let first = start.saturating_sub(1) as usize;
        let last = end as usize;
        let text = if first < lines.len() {
            lines[first..lines.len().min(last)].concat()
        } else {
            String::new()
        };
        // Report truncation whenever the caller did not receive everything
        // they could have: either the start is past the end of the file, the
        // explicit range was cut short (by the file end or the line cap), or
        // the implicit default window did not reach the end of the file.
        let line_truncated = start > available_end
            || match input.end_line {
                Some(requested) => end < requested,
                None => available_end > end,
            };
        serialize_result(
            json!({
                "text": text,
                "truncated": result.truncated,
                "original_bytes": result.original_bytes,
                "retained_bytes": result.retained_bytes,
                "start_line": start,
                "end_line": end,
                "line_truncated": line_truncated,
            }),
            ctx.output_limit.0,
        )
    }
}

fn serialize_result(mut value: Value, limit: usize) -> Result<String, String> {
    let mut encoded = serde_json::to_string(&value)
        .map_err(|error| ReadTool::error("serialization_failed", error.to_string()))?;
    if encoded.len() <= limit {
        return Ok(encoded);
    }

    let Some(text) = value
        .get_mut("text")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
    else {
        return Err(ReadTool::error(
            "output_limit",
            "serialized read result exceeds the tool output limit",
        ));
    };
    let mut keep = text.len();
    while encoded.len() > limit && keep > 0 {
        // Shrink proportionally instead of trimming a fixed number of bytes
        // per iteration: JSON escaping can inflate the encoded size by more
        // than the fixed step, which degrades to O(n^2) for heavily escaped
        // text. Scaling by `limit / encoded` converges in a couple of rounds.
        let next = keep.saturating_mul(limit) / encoded.len().max(1);
        let next = if next >= keep { keep - 1 } else { next };
        let mut next = next;
        while next > 0 && !text.is_char_boundary(next) {
            next -= 1;
        }
        keep = next;
        value["text"] = Value::String(text[..keep].to_owned());
        value["truncated"] = Value::Bool(true);
        encoded = serde_json::to_string(&value)
            .map_err(|error| ReadTool::error("serialization_failed", error.to_string()))?;
    }
    if encoded.len() > limit {
        return Err(ReadTool::error(
            "output_limit",
            "serialized read result exceeds the tool output limit",
        ));
    }
    Ok(encoded)
}
