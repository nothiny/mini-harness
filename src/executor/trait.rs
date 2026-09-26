use crate::error::{EditFileError, ProcessError, ReadFileError};
use crate::runtime::types::{ByteLimit, WorkspaceRoot};
use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug)]
pub struct ReadFileRequest {
    pub path: String,
    pub max_bytes: ByteLimit,
}
#[derive(Clone, Debug)]
pub struct ReadFileResult {
    pub text: String,
    pub truncated: bool,
    pub original_bytes: usize,
    pub retained_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct ProcessRequest {
    pub command: String,
    pub timeout: Duration,
    pub max_stdout_bytes: ByteLimit,
    pub max_stderr_bytes: ByteLimit,
    pub max_combined_bytes: ByteLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessOutput {
    pub text: String,
    pub truncated: bool,
    pub original_bytes: u64,
    pub retained_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessResult {
    pub stdout: ProcessOutput,
    pub stderr: ProcessOutput,
    pub exit_code: Option<i32>,
    /// Signal that terminated the process on Unix, if any.
    pub signal: Option<i32>,
    pub timed_out: bool,
}

#[derive(Clone, Debug)]
pub struct EditFileRequest {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub expected_hash: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EditFileResult {
    pub old_hash: String,
    pub new_hash: String,
    pub replacements: usize,
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn read_file(&self, request: ReadFileRequest) -> Result<ReadFileResult, ReadFileError>;

    async fn run_process(
        &self,
        _request: ProcessRequest,
        _cancel: CancellationToken,
    ) -> Result<ProcessResult, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn edit_file(&self, _request: EditFileRequest) -> Result<EditFileResult, EditFileError> {
        Err(EditFileError::Unsupported)
    }

    async fn edit_file_with_cancel(
        &self,
        request: EditFileRequest,
        _cancel: CancellationToken,
    ) -> Result<EditFileResult, EditFileError> {
        self.edit_file(request).await
    }

    fn workspace_root(&self) -> Option<WorkspaceRoot> {
        None
    }
}
