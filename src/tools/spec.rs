use crate::executor::Executor;
use crate::runtime::types::{ByteLimit, ToolName, WorkspaceRoot};
use crate::runtime::{ExecutionId, ToolCallId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// The JSON Schema shown to a model for a tool's arguments.
pub type JsonSchema = serde_json::Value;

/// Maximum serialized size of one model-visible tool specification.
pub const MAX_TOOL_SPEC_BYTES: usize = 16 * 1024;

/// The broad permission class a tool requires.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
    pub parameters: JsonSchema,
    pub risk: RiskClass,
}

/// Capabilities granted to one tool invocation.
pub struct ToolContext<'a> {
    pub executor: &'a dyn Executor,
    pub cancel: CancellationToken,
    pub workspace: Option<WorkspaceRoot>,
    pub tool_call_id: ToolCallId,
    pub execution_id: ExecutionId,
    pub output_limit: ByteLimit,
}

/// A model-visible operation. Implementations validate their own arguments and
/// use only the capabilities in `ToolContext` to perform work.
///
/// # Error contract
///
/// `execute` reports failures as a model-readable JSON string
/// (`{"error":{"kind","message","details"}}`) rather than a typed error.
/// This is deliberate (ADR-007 in docs/design.md): the value is handed back to
/// the model as tool output, so it must be self-describing on the wire, and a
/// typed enum would have to be flattened into exactly this JSON anyway.
/// Typed errors remain the contract at the executor and runtime boundaries
/// (`ReadFileError`, `EditFileError`, `ProcessError`, `HarnessError`); only
/// the model-facing layer serializes to strings.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Performs cheap, side-effect-free validation before policy approval or
    /// execution. Implementations can use this to reject malformed model
    /// arguments without asking the user to approve a call that can never
    /// run. Runtime execution must still validate again because tool inputs
    /// can come from restored durable state.
    fn validate_input(&self, _input: &JsonSchema) -> Result<(), String> {
        Ok(())
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> Result<String, String>;
}
