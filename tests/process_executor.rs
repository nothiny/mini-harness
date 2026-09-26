use std::time::Duration;

use mini_harness::{
    ProcessError,
    executor::{ProcessRequest, run_process},
    runtime::ByteLimit,
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn request(command: impl Into<String>) -> ProcessRequest {
    ProcessRequest {
        command: command.into(),
        timeout: Duration::from_secs(5),
        max_stdout_bytes: ByteLimit(64 * 1024),
        max_stderr_bytes: ByteLimit(64 * 1024),
        max_combined_bytes: ByteLimit(128 * 1024),
    }
}

#[tokio::test]
async fn captures_stdout_and_stderr_and_exit_code() {
    let directory = tempdir().unwrap();
    let command = if cfg!(windows) {
        "echo output & echo diagnostic 1>&2"
    } else {
        "printf output; printf diagnostic >&2"
    };
    let result = run_process(directory.path(), request(command), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert_eq!(result.stdout.text.trim(), "output");
    assert_eq!(result.stderr.text.trim(), "diagnostic");
    assert_eq!(
        result.stdout.original_bytes as usize,
        result.stdout.text.len()
    );
    assert_eq!(
        result.stderr.original_bytes as usize,
        result.stderr.text.len()
    );
}

#[tokio::test]
async fn nonzero_exit_is_returned_as_a_normal_result() {
    let directory = tempdir().unwrap();
    let command = if cfg!(windows) {
        "echo failed 1>&2 & exit /b 7"
    } else {
        "printf failed >&2; exit 7"
    };
    let result = run_process(directory.path(), request(command), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.exit_code, Some(7));
    assert_eq!(result.stderr.text.trim(), "failed");
    assert!(!result.timed_out);
}

#[cfg(unix)]
#[tokio::test]
async fn drains_large_output_after_retention_limits() {
    let directory = tempdir().unwrap();
    let mut process = request("yes x | head -c 200000; yes y | head -c 200000 >&2");
    process.max_stdout_bytes = ByteLimit(1024);
    process.max_stderr_bytes = ByteLimit(2048);
    process.max_combined_bytes = ByteLimit(2500);
    let result = run_process(directory.path(), process, CancellationToken::new())
        .await
        .unwrap();

    assert!(!result.timed_out);
    assert!(result.stdout.original_bytes >= 200_000);
    assert!(result.stderr.original_bytes >= 200_000);
    assert!(result.stdout.truncated);
    assert!(result.stderr.truncated);
    assert!(result.stdout.retained_bytes <= 1024);
    assert!(result.stderr.retained_bytes <= 2048);
    assert!(result.stdout.retained_bytes + result.stderr.retained_bytes <= 2500);
}

#[cfg(unix)]
#[tokio::test]
async fn truncation_does_not_split_utf8() {
    let directory = tempdir().unwrap();
    let command = "printf 'éé'";
    let mut process = request(command);
    process.max_stdout_bytes = ByteLimit(3);
    process.max_combined_bytes = ByteLimit(3);
    let result = run_process(directory.path(), process, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(result.stdout.text, "é");
    assert_eq!(result.stdout.retained_bytes, "é".len());
    assert!(result.stdout.truncated);
}

#[cfg(unix)]
#[tokio::test]
async fn command_runs_with_workspace_as_cwd() {
    let directory = tempdir().unwrap();
    let result = run_process(directory.path(), request("pwd"), CancellationToken::new())
        .await
        .unwrap();
    let expected = tokio::fs::canonicalize(directory.path()).await.unwrap();
    assert_eq!(result.stdout.text.trim(), expected.to_string_lossy());
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_process_group_and_reports_timeout() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("timeout-marker");
    let command = format!(
        "(sleep 30; echo alive > '{}') & wait",
        marker.to_string_lossy()
    );
    let mut process = request(command);
    process.timeout = Duration::from_millis(100);
    let result = run_process(directory.path(), process, CancellationToken::new())
        .await
        .unwrap();

    assert!(result.timed_out);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_process_group_and_returns_cancelled() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("cancel-marker");
    let command = format!(
        "(sleep 30; echo alive > '{}') & wait",
        marker.to_string_lossy()
    );
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let root = directory.path().to_path_buf();
    let task = tokio::spawn(async move { run_process(&root, request(command), task_cancel).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let error = task.await.unwrap().unwrap_err();

    assert_eq!(error, ProcessError::Cancelled);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn reports_signal_termination() {
    let directory = tempdir().unwrap();
    let result = run_process(
        directory.path(),
        request("kill -TERM $$"),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, None);
    assert_eq!(result.signal, Some(libc::SIGTERM));
    assert!(!result.timed_out);
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_also_covers_background_processes_holding_pipes() {
    let directory = tempdir().unwrap();
    let mut process = request("sleep 30 &");
    process.timeout = Duration::from_millis(100);
    let started = tokio::time::Instant::now();
    let result = run_process(directory.path(), process, CancellationToken::new())
        .await
        .unwrap();

    assert!(result.timed_out);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_also_covers_background_processes_holding_pipes() {
    let directory = tempdir().unwrap();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let root = directory.path().to_path_buf();
    let started = tokio::time::Instant::now();
    let task =
        tokio::spawn(async move { run_process(&root, request("sleep 30 &"), task_cancel).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    assert_eq!(task.await.unwrap().unwrap_err(), ProcessError::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));
}
