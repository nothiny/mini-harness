use std::time::Duration;

use mini_harness::{
    executor::{EditFileRequest, Executor, LocalExecutor, ProcessRequest},
    runtime::ByteLimit,
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn local_executor_runs_process_in_workspace() {
    let directory = tempdir().unwrap();
    let result = Executor::run_process(
        &LocalExecutor::new(directory.path().to_path_buf()),
        ProcessRequest {
            command: if cfg!(windows) {
                "echo workspace"
            } else {
                "pwd"
            }
            .into(),
            timeout: Duration::from_secs(5),
            max_stdout_bytes: ByteLimit(1024),
            max_stderr_bytes: ByteLimit(1024),
            max_combined_bytes: ByteLimit(2048),
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();

    if cfg!(windows) {
        assert_eq!(result.stdout.text.trim(), "workspace");
    } else {
        let expected = tokio::fs::canonicalize(directory.path()).await.unwrap();
        assert_eq!(result.stdout.text.trim(), expected.to_string_lossy());
    }
}

#[tokio::test]
async fn local_executor_edits_file_atomically() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("note.txt");
    tokio::fs::write(&path, "before").await.unwrap();
    let executor = LocalExecutor::new(directory.path().to_path_buf());

    let result = executor
        .edit_file(EditFileRequest {
            path: "note.txt".into(),
            old_text: "before".into(),
            new_text: "after".into(),
            expected_hash: None,
        })
        .await
        .unwrap();

    assert_eq!(result.replacements, 1);
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "after");
}
