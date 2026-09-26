use std::path::Path;

use mini_harness::{
    error::ReadFileError,
    executor::{Executor, LocalExecutor, ReadFileRequest},
    runtime::ByteLimit,
};
use tempfile::tempdir;

fn request(path: impl Into<String>, max_bytes: usize) -> ReadFileRequest {
    ReadFileRequest {
        path: path.into(),
        max_bytes: ByteLimit(max_bytes),
    }
}

#[tokio::test]
async fn read_is_bounded_and_reports_truncation() {
    let dir = tempdir().unwrap();
    tokio::fs::write(dir.path().join("text.txt"), "abcdefghij")
        .await
        .unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor.read_file(request("text.txt", 4)).await.unwrap();

    assert_eq!(result.text, "abcd");
    assert!(result.truncated);
    assert_eq!(result.original_bytes, 10);
    assert_eq!(result.retained_bytes, 4);
}

#[tokio::test]
async fn read_accepts_absolute_path_inside_root() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("text.txt");
    tokio::fs::write(&file, "hello").await.unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor
        .read_file(request(file.to_string_lossy().into_owned(), 100))
        .await
        .unwrap();

    assert_eq!(result.text, "hello");
    assert!(!result.truncated);
}

#[tokio::test]
async fn read_accepts_parent_segments_that_stay_inside_root() {
    let dir = tempdir().unwrap();
    tokio::fs::create_dir(dir.path().join("nested"))
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("text.txt"), "hello")
        .await
        .unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor
        .read_file(request("nested/../text.txt", 100))
        .await
        .unwrap();

    assert_eq!(result.text, "hello");
}

#[tokio::test]
async fn read_rejects_missing_files_and_directories() {
    let dir = tempdir().unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let missing = executor.read_file(request("missing.txt", 10)).await;
    assert!(matches!(missing, Err(ReadFileError::NotFound { .. })));

    let directory = executor.read_file(request(".", 10)).await;
    assert!(matches!(directory, Err(ReadFileError::IsDirectory { .. })));
}

#[tokio::test]
async fn read_rejects_traversal() {
    let dir = tempdir().unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor.read_file(request("../outside.txt", 10)).await;

    assert!(matches!(
        result,
        Err(ReadFileError::OutsideWorkspace { .. })
    ));
}

#[tokio::test]
async fn read_rejects_missing_absolute_path_outside_root() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let executor = LocalExecutor::new(root.path().to_path_buf());
    let path = outside.path().join("missing.txt");

    let result = executor
        .read_file(request(path.to_string_lossy().into_owned(), 10))
        .await;

    assert!(matches!(
        result,
        Err(ReadFileError::OutsideWorkspace { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn read_rejects_symlink_that_resolves_outside_root() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    tokio::fs::write(outside.path().join("secret.txt"), "secret")
        .await
        .unwrap();
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("link.txt"),
    )
    .unwrap();
    let executor = LocalExecutor::new(root.path().to_path_buf());

    let result = executor.read_file(request("link.txt", 100)).await;

    assert!(matches!(
        result,
        Err(ReadFileError::OutsideWorkspace { .. })
    ));
}

#[tokio::test]
async fn read_rejects_binary_content() {
    let dir = tempdir().unwrap();
    tokio::fs::write(dir.path().join("binary.bin"), [0_u8, 1, 2, 3])
        .await
        .unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor.read_file(request("binary.bin", 100)).await;

    assert!(matches!(result, Err(ReadFileError::BinaryFile { .. })));
}

#[tokio::test]
async fn read_checks_binary_lookahead_even_when_limit_is_zero() {
    let dir = tempdir().unwrap();
    tokio::fs::write(dir.path().join("binary.bin"), [0_u8, 1, 2, 3])
        .await
        .unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor.read_file(request("binary.bin", 0)).await;

    assert!(matches!(result, Err(ReadFileError::BinaryFile { .. })));
}

#[tokio::test]
async fn read_rejects_invalid_utf8() {
    let dir = tempdir().unwrap();
    tokio::fs::write(dir.path().join("invalid.bin"), [0xff_u8, 0xfe])
        .await
        .unwrap();
    let executor = LocalExecutor::new(dir.path().to_path_buf());

    let result = executor.read_file(request("invalid.bin", 100)).await;

    assert!(matches!(result, Err(ReadFileError::BinaryFile { .. })));
}

#[test]
fn error_paths_are_path_values() {
    let path = Path::new("outside");
    let error = ReadFileError::OutsideWorkspace {
        path: path.to_path_buf(),
    };
    assert_eq!(error.to_string(), "path \"outside\" escapes the workspace");
}
