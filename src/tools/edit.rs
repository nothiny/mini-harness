use super::spec::{RiskClass, Tool, ToolContext, ToolSpec};
use crate::{executor::EditFileRequest, runtime::ToolName};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_EDIT_TEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
    expected_hash: Option<String>,
}

pub struct EditTool;

impl EditTool {
    fn error(kind: &str, message: impl Into<String>) -> String {
        json!({"error": {"kind": kind, "message": message.into()}}).to_string()
    }
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName("edit".into()),
            description: "Replace one exact text occurrence in a workspace file".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "old_text": {"type": "string", "minLength": 1},
                    "new_text": {"type": "string"},
                    "expected_hash": {"type": "string"}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
            risk: RiskClass::Write,
        }
    }

    async fn execute(&self, input: Value, ctx: ToolContext<'_>) -> Result<String, String> {
        let input: EditInput = serde_json::from_value(input).map_err(|error| {
            Self::error("invalid_input", format!("invalid edit input: {error}"))
        })?;
        if input.path.trim().is_empty() {
            return Err(Self::error("invalid_input", "path must not be empty"));
        }
        if input.old_text.is_empty() {
            return Err(Self::error("invalid_input", "old_text must not be empty"));
        }
        if input.old_text.len() > MAX_EDIT_TEXT_BYTES {
            return Err(Self::error(
                "invalid_input",
                format!("old_text must not exceed {MAX_EDIT_TEXT_BYTES} bytes"),
            ));
        }
        if input.new_text.len() > MAX_EDIT_TEXT_BYTES {
            return Err(Self::error(
                "invalid_input",
                format!("new_text must not exceed {MAX_EDIT_TEXT_BYTES} bytes"),
            ));
        }
        if ctx.cancel.is_cancelled() {
            return Err(Self::error("cancelled", "edit cancelled before execution"));
        }
        // File replacement is a short atomic transaction. Let the executor
        // finish it so cancellation cannot drop the future between temp-file
        // creation and rename.
        let result = ctx
            .executor
            .edit_file_with_cancel(
                EditFileRequest {
                    path: input.path,
                    old_text: input.old_text,
                    new_text: input.new_text,
                    expected_hash: input.expected_hash,
                },
                ctx.cancel.child_token(),
            )
            .await;
        let result = result.map_err(|error| {
            let kind = if matches!(error, crate::error::EditFileError::Cancelled) {
                "cancelled"
            } else {
                "edit_failed"
            };
            let details = serde_json::to_value(&error)
                .unwrap_or_else(|_| json!({"message": error.to_string()}));
            json!({
                "error": {
                    "kind": kind,
                    "message": error.to_string(),
                    "details": details
                }
            })
            .to_string()
        })?;
        Ok(serde_json::to_string(&result).unwrap_or_else(|_| "{}".into()))
    }
}
