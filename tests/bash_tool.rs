use async_trait::async_trait;
use mini_harness::{
    error::{EditFileError, ProcessError, ReadFileError},
    executor::{
        EditFileRequest, EditFileResult, Executor, ProcessRequest, ProcessResult, ReadFileRequest,
        ReadFileResult,
    },
    runtime::{ByteLimit, ExecutionId, ToolCallId},
    tools::{BashTool, RiskClass, Tool, ToolContext},
};
use serde_json::{Value, json};
use std::{sync::Mutex, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct FakeExecutor {
    request: Mutex<Option<ProcessRequest>>,
    cancel: Mutex<Option<CancellationToken>>,
    result: Mutex<Option<Result<ProcessResult, ProcessError>>>,
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn read_file(&self, _request: ReadFileRequest) -> Result<ReadFileResult, ReadFileError> {
        unreachable!()
    }

    async fn run_process(
        &self,
        request: ProcessRequest,
        cancel: CancellationToken,
    ) -> Result<ProcessResult, ProcessError> {
        *self.request.lock().unwrap() = Some(request);
        *self.cancel.lock().unwrap() = Some(cancel);
        self.result.lock().unwrap().take().unwrap()
    }

    async fn edit_file(&self, _request: EditFileRequest) -> Result<EditFileResult, EditFileError> {
        unreachable!()
    }
}

fn context<'a>(executor: &'a FakeExecutor, cancel: CancellationToken) -> ToolContext<'a> {
    ToolContext {
        executor,
        cancel,
        workspace: None,
        tool_call_id: ToolCallId::new(),
        execution_id: ExecutionId::new(),
        output_limit: ByteLimit(1024),
    }
}

fn successful_result() -> ProcessResult {
    ProcessResult {
        stdout: mini_harness::executor::ProcessOutput {
            text: "hello\n".into(),
            truncated: false,
            original_bytes: 6,
            retained_bytes: 6,
        },
        stderr: mini_harness::executor::ProcessOutput {
            text: String::new(),
            truncated: false,
            original_bytes: 0,
            retained_bytes: 0,
        },
        exit_code: Some(0),
        signal: None,
        timed_out: false,
    }
}

fn make_executor(result: Result<ProcessResult, ProcessError>) -> FakeExecutor {
    FakeExecutor {
        result: Mutex::new(Some(result)),
        ..FakeExecutor::default()
    }
}

#[tokio::test]
async fn bash_uses_executor_timeout_cancellation_and_output_limit() {
    let executor = make_executor(Ok(successful_result()));
    let cancel = CancellationToken::new();
    let response = BashTool::default()
        .execute(
            json!({"command": "printf hello", "timeout_ms": 500}),
            context(&executor, cancel.clone()),
        )
        .await
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["stdout"]["text"], "hello\n");
    assert_eq!(response["exit_code"], 0);

    let request = executor.request.lock().unwrap().clone().unwrap();
    assert_eq!(request.command, "printf hello");
    assert_eq!(request.timeout, Duration::from_millis(500));
    assert_eq!(request.max_stdout_bytes, ByteLimit(1024));
    assert_eq!(request.max_stderr_bytes, ByteLimit(1024));
    assert_eq!(request.max_combined_bytes, ByteLimit(1024));
    let child = executor.cancel.lock().unwrap().clone().unwrap();
    assert!(!child.is_cancelled());
    cancel.cancel();
    assert!(child.is_cancelled());
    assert_eq!(BashTool::default().spec().risk, RiskClass::Execute);
}

#[tokio::test]
async fn bash_truncation_converges_for_heavily_escaped_output() {
    // Quotes and newlines each expand during JSON encoding; the proportional
    // shrink must fit the serialized result under a tight limit.
    let mut result = successful_result();
    result.stdout.text = "\"quoted\"\n".repeat(300);
    result.stdout.original_bytes = result.stdout.text.len() as u64;
    result.stdout.retained_bytes = result.stdout.text.len();
    let executor = make_executor(Ok(result));
    let limit = 384;
    let response = BashTool::default()
        .execute(
            json!({"command": "printf quotes"}),
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
        response.len() <= limit,
        "response exceeded limit: {}",
        response.len()
    );
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["stdout"]["truncated"], true);
}

#[tokio::test]
async fn bash_applies_default_timeout_and_rejects_invalid_arguments() {
    let executor = make_executor(Ok(successful_result()));
    BashTool::default()
        .execute(
            json!({"command": "true"}),
            context(&executor, CancellationToken::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        executor.request.lock().unwrap().as_ref().unwrap().timeout,
        Duration::from_secs(30)
    );

    for input in [
        json!({"command": "  "}),
        json!({"command": "true", "timeout_ms": 0}),
        json!({"command": "true", "timeout_ms": 600001}),
        json!({"command": "true", "unknown": true}),
    ] {
        let executor = make_executor(Ok(successful_result()));
        let error = BashTool::default()
            .execute(input, context(&executor, CancellationToken::new()))
            .await
            .unwrap_err();
        let error: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(error["error"]["kind"], "invalid_input");
        assert!(executor.request.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn bash_returns_structured_executor_errors_and_cancellation() {
    let executor = make_executor(Err(ProcessError::Spawn {
        message: "no shell".into(),
    }));
    let error = BashTool::default()
        .execute(
            json!({"command": "true"}),
            context(&executor, CancellationToken::new()),
        )
        .await
        .unwrap_err();
    let error: Value = serde_json::from_str(&error).unwrap();
    assert_eq!(error["error"]["kind"], "process_failed");
    assert_eq!(error["error"]["details"]["Spawn"]["message"], "no shell");

    let executor = make_executor(Ok(successful_result()));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = BashTool::default()
        .execute(json!({"command": "true"}), context(&executor, cancel))
        .await
        .unwrap_err();
    let error: Value = serde_json::from_str(&error).unwrap();
    assert_eq!(error["error"]["kind"], "cancelled");
    assert!(executor.request.lock().unwrap().is_none());
}

#[tokio::test]
async fn zero_configured_default_timeout_disables_the_default_not_the_command() {
    // A zero duration means "limit disabled" everywhere else in the config
    // surface; for the bash default timeout it must mean "no default cap",
    // not "every command instantly times out".
    let executor = make_executor(Ok(successful_result()));
    BashTool::with_default_timeout(Duration::ZERO)
        .execute(
            json!({"command": "true"}),
            context(&executor, CancellationToken::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        executor.request.lock().unwrap().as_ref().unwrap().timeout,
        BashTool::with_default_timeout(Duration::ZERO).max_timeout,
        "zero default must fall back to the hard ceiling, not zero"
    );
}
