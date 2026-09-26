use async_trait::async_trait;
use mini_harness::error::ReadFileError;
use mini_harness::executor::{Executor, ReadFileRequest, ReadFileResult};
use mini_harness::runtime::{ByteLimit, ExecutionId, ToolCallId, ToolName};
use mini_harness::tools::{
    ReadTool, RiskClass, Tool, ToolContext, ToolRegistry, ToolRegistryError, ToolSpec,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct NoopTool {
    name: &'static str,
}

struct LargeSpecTool;

#[async_trait]
impl Tool for LargeSpecTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName("large".into()),
            description: "x".repeat(mini_harness::tools::MAX_TOOL_SPEC_BYTES + 1),
            parameters: json!({"type": "object"}),
            risk: RiskClass::Read,
        }
    }

    async fn execute(&self, _input: Value, _ctx: ToolContext<'_>) -> Result<String, String> {
        Ok("ok".into())
    }
}

#[async_trait]
impl Tool for NoopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName(self.name.into()),
            description: format!("{} tool", self.name),
            parameters: json!({"type": "object"}),
            risk: RiskClass::Read,
        }
    }

    async fn execute(&self, _input: Value, _ctx: ToolContext<'_>) -> Result<String, String> {
        Ok("ok".into())
    }
}

#[derive(Clone)]
struct StubExecutor {
    result: Result<ReadFileResult, ReadFileError>,
}

#[async_trait]
impl Executor for StubExecutor {
    async fn read_file(&self, _request: ReadFileRequest) -> Result<ReadFileResult, ReadFileError> {
        self.result.clone()
    }
}

fn text_executor(text: &str) -> StubExecutor {
    StubExecutor {
        result: Ok(ReadFileResult {
            text: text.into(),
            truncated: false,
            original_bytes: text.len(),
            retained_bytes: text.len(),
        }),
    }
}

#[test]
fn registry_has_deterministic_specs_and_structured_lookup_errors() {
    let mut registry = ToolRegistry::default();
    registry.register(NoopTool { name: "zeta" }).unwrap();
    registry.register(NoopTool { name: "alpha" }).unwrap();

    let specs = registry.specs();
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name.0.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(specs[0].parameters, json!({"type": "object"}));
    assert_eq!(specs[0].risk, RiskClass::Read);
    assert!(matches!(
        registry.register(NoopTool { name: "alpha" }),
        Err(ToolRegistryError::Duplicate { name }) if name == "alpha"
    ));
    assert!(matches!(
        registry.lookup("missing"),
        Err(ToolRegistryError::Unknown { name }) if name == "missing"
    ));
    assert!(matches!(
        registry.register(NoopTool { name: " " }),
        Err(ToolRegistryError::EmptyName)
    ));
}

#[test]
fn registry_rejects_oversized_tool_specs() {
    let mut registry = ToolRegistry::default();
    assert!(matches!(
        registry.register(LargeSpecTool),
        Err(ToolRegistryError::SpecTooLarge { .. })
    ));
}

#[tokio::test]
async fn read_returns_requested_lines_and_metadata() {
    let executor = text_executor("one\ntwo\nthree\n");
    let tool = ReadTool { max_bytes: 100 };
    let output = tool
        .execute(
            json!({"path": "notes.txt", "start_line": 2, "end_line": 2}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["text"], "two\n");
    assert_eq!(output["start_line"], 2);
    assert_eq!(output["end_line"], 2);
    assert_eq!(output["truncated"], false);
}

#[tokio::test]
async fn read_caps_serialized_result_to_output_limit() {
    let executor = text_executor(&"line\n".repeat(100));
    let limit = 200;
    let output = ReadTool { max_bytes: 1024 }
        .execute(
            json!({"path": "notes.txt"}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(limit),
            },
        )
        .await
        .unwrap();

    assert!(output.len() <= limit);
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["truncated"], true);
}

#[tokio::test]
async fn read_truncation_converges_for_heavily_escaped_output() {
    // Every quote doubles in the JSON encoding, so the encoded size is
    // roughly twice the raw text. Proportional shrinking must still land
    // under the limit in a bounded number of rounds (the old fixed-step
    // trim needed one round per ~excess bytes).
    let executor = text_executor(&"\"quoted\"\n".repeat(400));
    let limit = 512;
    let output = ReadTool {
        max_bytes: 16 * 1024,
    }
    .execute(
        json!({"path": "notes.txt"}),
        ToolContext {
            executor: &executor,
            cancel: CancellationToken::new(),
            workspace: None,
            tool_call_id: ToolCallId::new(),
            execution_id: ExecutionId::new(),
            output_limit: ByteLimit(limit),
        },
    )
    .await
    .unwrap();

    assert!(
        output.len() <= limit,
        "output exceeded limit: {}",
        output.len()
    );
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["truncated"], true);
    assert!(output["text"].as_str().is_some_and(|text| !text.is_empty()));
}

#[tokio::test]
async fn read_reports_an_out_of_range_start_without_underflowing_end_line() {
    let executor = text_executor("one\n");
    let output = ReadTool { max_bytes: 100 }
        .execute(
            json!({"path": "notes.txt", "start_line": 10}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["text"], "");
    assert_eq!(output["start_line"], 10);
    assert_eq!(output["end_line"], 10);
    assert_eq!(output["line_truncated"], true);
}

#[tokio::test]
async fn read_rejects_unknown_fields_and_cancelled_calls() {
    let executor = text_executor("content");
    let tool = ReadTool { max_bytes: 100 };
    let invalid = tool
        .execute(
            json!({"path": "notes.txt", "unexpected": true}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap_err();
    let invalid: Value = serde_json::from_str(&invalid).unwrap();
    assert_eq!(invalid["error"]["kind"], "invalid_input");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = tool
        .execute(
            json!({"path": "notes.txt"}),
            ToolContext {
                executor: &executor,
                cancel,
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap_err();
    let cancelled: Value = serde_json::from_str(&cancelled).unwrap();
    assert_eq!(cancelled["error"]["kind"], "cancelled");
}

#[tokio::test]
async fn read_clamps_oversized_ranges_instead_of_rejecting_them() {
    let executor = text_executor(&(1..=400).map(|n| format!("line {n}\n")).collect::<String>());
    let tool = ReadTool {
        max_bytes: 64 * 1024,
    };

    // Explicit range larger than the 200-line cap is clamped, not rejected.
    let output = tool
        .execute(
            json!({"path": "notes.txt", "start_line": 1, "end_line": 400}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["start_line"], 1);
    assert_eq!(output["end_line"], 200);
    assert_eq!(output["line_truncated"], true);
    assert_eq!(output["text"].as_str().unwrap().lines().count(), 200);

    // An explicit range that ends inside the file but exceeds the cap is also
    // flagged as truncated.
    let output = tool
        .execute(
            json!({"path": "notes.txt", "start_line": 150, "end_line": 399}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["start_line"], 150);
    assert_eq!(output["end_line"], 349);
    assert_eq!(output["line_truncated"], true);
}

#[tokio::test]
async fn read_does_not_flag_truncation_for_ranges_fully_inside_the_file() {
    let executor = text_executor("one\ntwo\nthree\n");
    let output = ReadTool { max_bytes: 100 }
        .execute(
            json!({"path": "notes.txt", "start_line": 1, "end_line": 2}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["line_truncated"], false);
}

#[tokio::test]
async fn read_preserves_structured_executor_errors() {
    let executor = StubExecutor {
        result: Err(ReadFileError::OutsideWorkspace {
            path: PathBuf::from("../secret.txt"),
        }),
    };
    let output = ReadTool { max_bytes: 100 }
        .execute(
            json!({"path": "../secret.txt"}),
            ToolContext {
                executor: &executor,
                cancel: CancellationToken::new(),
                workspace: None,
                tool_call_id: ToolCallId::new(),
                execution_id: ExecutionId::new(),
                output_limit: ByteLimit(usize::MAX),
            },
        )
        .await
        .unwrap_err();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["error"]["kind"], "read_failed");
    assert!(output["error"]["details"]["OutsideWorkspace"].is_object());
}
