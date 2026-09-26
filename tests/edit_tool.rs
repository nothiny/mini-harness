use mini_harness::{
    error::EditFileError,
    executor::{EditFileRequest, filesystem::edit_file, filesystem::edit_file_with_cancel},
};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio_util::sync::CancellationToken;

fn request(path: &str, old_text: &str, new_text: &str) -> EditFileRequest {
    EditFileRequest {
        path: path.into(),
        old_text: old_text.into(),
        new_text: new_text.into(),
        expected_hash: None,
    }
}

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn replaces_one_exact_occurrence_and_reports_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "before OLD after").await.unwrap();

    let result = edit_file(dir.path(), request("file.txt", "OLD", "new"))
        .await
        .unwrap();

    assert_eq!(
        tokio::fs::read_to_string(path).await.unwrap(),
        "before new after"
    );
    assert_eq!(result.old_hash, sha256("before OLD after"));
    assert_eq!(result.new_hash, sha256("before new after"));
    assert_eq!(result.replacements, 1);
}

#[tokio::test]
async fn rejects_missing_and_ambiguous_patches_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "OLD and OLD").await.unwrap();

    assert!(matches!(
        edit_file(dir.path(), request("file.txt", "absent", "new")).await,
        Err(EditFileError::PatchNotFound { .. })
    ));
    assert!(matches!(
        edit_file(dir.path(), request("file.txt", "OLD", "new")).await,
        Err(EditFileError::AmbiguousPatch { .. })
    ));
    assert_eq!(
        tokio::fs::read_to_string(path).await.unwrap(),
        "OLD and OLD"
    );
}

#[tokio::test]
async fn rejects_overlapping_patch_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "aaa").await.unwrap();

    assert!(matches!(
        edit_file(dir.path(), request("file.txt", "aa", "x")).await,
        Err(EditFileError::AmbiguousPatch { .. })
    ));
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "aaa");
}

#[tokio::test]
async fn checks_expected_hash_and_workspace_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "OLD").await.unwrap();
    let mut stale = request("file.txt", "OLD", "new");
    stale.expected_hash = Some(sha256("stale"));

    assert!(matches!(
        edit_file(dir.path(), stale).await,
        Err(EditFileError::Conflict { .. })
    ));
    assert!(matches!(
        edit_file(dir.path(), request("../outside", "x", "y")).await,
        Err(EditFileError::OutsideWorkspace { .. })
    ));
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "OLD");
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlinks_that_escape_workspace() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("target.txt");
    tokio::fs::write(&outside_file, "OLD").await.unwrap();
    symlink(&outside_file, dir.path().join("link.txt")).unwrap();

    assert!(matches!(
        edit_file(dir.path(), request("link.txt", "OLD", "new")).await,
        Err(EditFileError::OutsideWorkspace { .. })
    ));
    assert_eq!(
        tokio::fs::read_to_string(outside_file).await.unwrap(),
        "OLD"
    );
}

#[tokio::test]
async fn preserves_existing_file_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "OLD").await.unwrap();
    let mut permissions = tokio::fs::metadata(&path).await.unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o640);
        tokio::fs::set_permissions(&path, permissions)
            .await
            .unwrap();
    }

    edit_file(dir.path(), request("file.txt", "OLD", "new"))
        .await
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(Path::new(&path))
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}

#[tokio::test]
async fn cancelled_edit_does_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "OLD").await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    assert_eq!(
        edit_file_with_cancel(dir.path(), request("file.txt", "OLD", "new"), cancel).await,
        Err(EditFileError::Cancelled)
    );
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), "OLD");
}

#[tokio::test]
async fn rejects_oversized_edit_input_before_reading_file() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("file.txt"), "OLD")
        .await
        .unwrap();
    let oversized = "x".repeat(4 * 1024 * 1024 + 1);

    assert!(matches!(
        edit_file(dir.path(), request("file.txt", &oversized, "new")).await,
        Err(EditFileError::TooLarge { field, .. }) if field == "old_text"
    ));
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("file.txt"))
            .await
            .unwrap(),
        "OLD"
    );
}

#[tokio::test]
async fn serializes_concurrent_hash_guarded_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    tokio::fs::write(&path, "OLD").await.unwrap();
    let expected_hash = sha256("OLD");
    let mut first = request("file.txt", "OLD", "first");
    first.expected_hash = Some(expected_hash.clone());
    let mut second = request("file.txt", "OLD", "second");
    second.expected_hash = Some(expected_hash);

    let (first_result, second_result) =
        tokio::join!(edit_file(dir.path(), first), edit_file(dir.path(), second));
    let first_ok = first_result.is_ok();
    let second_ok = second_result.is_ok();
    assert_ne!(first_ok, second_ok);
    if first_ok {
        assert!(matches!(second_result, Err(EditFileError::Conflict { .. })));
    } else {
        assert!(matches!(first_result, Err(EditFileError::Conflict { .. })));
    }
    let contents = tokio::fs::read_to_string(path).await.unwrap();
    assert!(contents == "first" || contents == "second");
}
