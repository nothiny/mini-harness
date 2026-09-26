use super::spec::{RiskClass, Tool, ToolContext, ToolSpec};
use crate::{
    error::ProcessError,
    executor::{ProcessRequest, ProcessResult},
    runtime::types::{ByteLimit, ToolName},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_COMMAND_BYTES: usize = 64 * 1024;

/// `bash` tool with configurable timeouts (design §16 `[execution]`).
///
/// Defaults keep the historical 30s/10min pair; `new` builds the documented
/// configuration (configurable default, hard 10-minute ceiling).
#[derive(Clone, Copy, Debug)]
pub struct BashTool {
    pub default_timeout: Duration,
    pub max_timeout: Duration,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            default_timeout: DEFAULT_TIMEOUT,
            max_timeout: MAX_TIMEOUT,
        }
    }
}

impl BashTool {
    /// A tool whose default timeout comes from configuration.
    pub fn with_default_timeout(default_timeout: Duration) -> Self {
        Self {
            default_timeout,
            max_timeout: MAX_TIMEOUT,
        }
    }
}

impl BashTool {
    fn error(kind: &str, message: impl Into<String>) -> String {
        json!({"error": {"kind": kind, "message": message.into()}}).to_string()
    }

    fn parse_input(&self, input: Value) -> Result<(String, Duration), String> {
        let object = input
            .as_object()
            .ok_or_else(|| Self::error("invalid_input", "input must be a JSON object"))?;
        if let Some(name) = object
            .keys()
            .find(|name| !["command", "timeout_ms"].contains(&name.as_str()))
        {
            return Err(Self::error(
                "invalid_input",
                format!("unknown field `{name}`"),
            ));
        }
        let command = object
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| Self::error("invalid_input", "command must be a string"))?;
        if command.trim().is_empty() {
            return Err(Self::error("invalid_input", "command must not be empty"));
        }
        if command.len() > MAX_COMMAND_BYTES {
            return Err(Self::error(
                "invalid_input",
                format!("command must not exceed {MAX_COMMAND_BYTES} bytes"),
            ));
        }
        let timeout = match object.get("timeout_ms") {
            None => {
                // Zero means "no configured default": fall back to the hard
                // ceiling instead of timing every command out instantly
                // (zero-duration = limit-disabled, like every other config
                // surface).
                if self.default_timeout.is_zero() {
                    self.max_timeout
                } else {
                    self.default_timeout
                }
            }
            Some(value) => {
                let millis = value.as_u64().filter(|millis| *millis > 0).ok_or_else(|| {
                    Self::error("invalid_input", "timeout_ms must be a positive integer")
                })?;
                let timeout = Duration::from_millis(millis);
                if timeout > self.max_timeout {
                    return Err(Self::error(
                        "invalid_input",
                        format!(
                            "timeout_ms must not exceed {}",
                            self.max_timeout.as_millis()
                        ),
                    ));
                }
                timeout
            }
        };
        Ok((command.to_owned(), timeout))
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName("bash".into()),
            description: "Run a shell command".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "minLength": 1, "maxLength": MAX_COMMAND_BYTES },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": self.max_timeout.as_millis() }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            risk: RiskClass::Execute,
        }
    }

    async fn execute(&self, input: Value, ctx: ToolContext<'_>) -> Result<String, String> {
        let (command, timeout) = self.parse_input(input)?;
        if ctx.cancel.is_cancelled() {
            return Err(Self::error(
                "cancelled",
                "command cancelled before execution",
            ));
        }
        let limit = ByteLimit(ctx.output_limit.0);
        let child_cancel = ctx.cancel.child_token();
        let result = ctx
            .executor
            .run_process(
                ProcessRequest {
                    command,
                    timeout,
                    max_stdout_bytes: limit,
                    max_stderr_bytes: limit,
                    max_combined_bytes: limit,
                },
                child_cancel,
            )
            .await;
        let result = result.map_err(|error| {
            let kind = match error {
                ProcessError::Cancelled => "cancelled",
                ProcessError::ConcurrencyLimit => "concurrency_limited",
                _ => "process_failed",
            };
            let details = serde_json::to_value(&error)
                .unwrap_or_else(|_| json!({"message": error.to_string()}));
            json!({
                "error": {
                    "kind": kind,
                    "message": error.to_string(),
                    "details": details,
                }
            })
            .to_string()
        })?;
        if ctx.cancel.is_cancelled() {
            return Err(Self::error("cancelled", "command cancelled"));
        }
        serialize_result(result, ctx.output_limit.0)
    }
}

fn serialize_result(mut result: ProcessResult, limit: usize) -> Result<String, String> {
    let mut encoded = serde_json::to_string(&result)
        .map_err(|error| BashTool::error("serialization_failed", error.to_string()))?;
    while encoded.len() > limit {
        let (first, second) = if result.stdout.text.len() >= result.stderr.text.len() {
            (&mut result.stdout, &mut result.stderr)
        } else {
            (&mut result.stderr, &mut result.stdout)
        };
        let Some(output) = [first, second]
            .into_iter()
            .find(|output| !output.text.is_empty())
        else {
            return Err(BashTool::error(
                "output_limit",
                "serialized process result exceeds the tool output limit",
            ));
        };
        // Shrink proportionally (see read.rs): JSON escaping can inflate the
        // encoded size well beyond the raw byte count, so a fixed trim step
        // would need one round per step for quote- or newline-heavy output.
        let current = output.text.len();
        let next = current.saturating_mul(limit) / encoded.len().max(1);
        let next = if next >= current { current - 1 } else { next };
        let mut next = next;
        while next > 0 && !output.text.is_char_boundary(next) {
            next -= 1;
        }
        output.text.truncate(next);
        output.truncated = true;
        output.retained_bytes = next;
        encoded = serde_json::to_string(&result)
            .map_err(|error| BashTool::error("serialization_failed", error.to_string()))?;
    }
    Ok(encoded)
}
