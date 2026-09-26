#[path = "filesystem_secure.rs"]
mod filesystem_secure;
pub(crate) use filesystem_secure::open_workspace_file;
use filesystem_secure::replace_file;

use crate::{
    error::EditFileError,
    executor::{EditFileRequest, EditFileResult},
};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
pub(crate) const MAX_EDIT_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_EDIT_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Replaces exactly one occurrence of `old_text` in a workspace file atomically.
pub async fn edit_file(
    root: &Path,
    request: EditFileRequest,
) -> Result<EditFileResult, EditFileError> {
    edit_file_with_cancel(root, request, CancellationToken::new()).await
}

pub async fn edit_file_with_cancel(
    root: &Path,
    request: EditFileRequest,
    cancel: CancellationToken,
) -> Result<EditFileResult, EditFileError> {
    if cancel.is_cancelled() {
        return Err(EditFileError::Cancelled);
    }
    let requested = Path::new(&request.path);
    if request.old_text.len() > MAX_EDIT_TEXT_BYTES {
        return Err(EditFileError::TooLarge {
            field: "old_text".into(),
            max_bytes: MAX_EDIT_TEXT_BYTES,
        });
    }
    if request.new_text.len() > MAX_EDIT_TEXT_BYTES {
        return Err(EditFileError::TooLarge {
            field: "new_text".into(),
            max_bytes: MAX_EDIT_TEXT_BYTES,
        });
    }
    let path = resolve(root, requested).await?;
    let _lock = acquire_edit_lock(&path, requested, cancel.clone()).await?;
    if cancel.is_cancelled() {
        return Err(EditFileError::Cancelled);
    }
    let (metadata, original) = read_workspace_file(root, &path, requested).await?;
    if metadata.len() > MAX_EDIT_FILE_BYTES || original.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(EditFileError::TooLarge {
            field: "file".into(),
            max_bytes: usize::try_from(MAX_EDIT_FILE_BYTES).unwrap_or(usize::MAX),
        });
    }
    let old_hash = hash(&original);
    if let Some(expected) = request.expected_hash.as_deref() {
        if expected != old_hash {
            return Err(EditFileError::Conflict {
                expected: expected.to_owned(),
                actual: old_hash,
            });
        }
    }
    let text = std::str::from_utf8(&original).map_err(|_| EditFileError::BinaryFile {
        path: requested.to_path_buf(),
    })?;
    let (occurrences, start) = find_occurrences(text, &request.old_text);
    if occurrences == 0 {
        return Err(EditFileError::PatchNotFound {
            path: requested.to_path_buf(),
        });
    }
    if occurrences != 1 {
        return Err(EditFileError::AmbiguousPatch {
            path: requested.to_path_buf(),
        });
    }

    let start = start.expect("one occurrence was counted");
    let mut replacement =
        String::with_capacity(text.len() - request.old_text.len() + request.new_text.len());
    replacement.push_str(&text[..start]);
    replacement.push_str(&request.new_text);
    replacement.push_str(&text[start + request.old_text.len()..]);
    let replacement = replacement.into_bytes();
    let new_hash = hash(&replacement);

    // Re-resolve and re-read immediately before commit so symlink/path changes and
    // concurrent content edits are reported as conflicts instead of overwritten.
    let current_path = resolve(root, requested).await?;
    if current_path != path {
        return Err(EditFileError::OutsideWorkspace {
            path: requested.to_path_buf(),
        });
    }
    let (_, current) = read_workspace_file(root, &path, requested).await?;
    if current.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(EditFileError::TooLarge {
            field: "file".into(),
            max_bytes: usize::try_from(MAX_EDIT_FILE_BYTES).unwrap_or(usize::MAX),
        });
    }
    let current_hash = hash(&current);
    if current_hash != old_hash {
        return Err(EditFileError::Conflict {
            expected: old_hash,
            actual: current_hash,
        });
    }

    if cancel.is_cancelled() {
        return Err(EditFileError::Cancelled);
    }

    let parent = path.parent().expect("canonical file has a parent");
    let mut temp_path = None;
    let mut created_file = None;
    for _ in 0..100 {
        let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".mini-harness-edit-{}-{id}.tmp",
            std::process::id()
        ));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(created) => {
                temp_path = Some(candidate);
                created_file = Some(created);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io(requested, error)),
        }
    }
    let temp_path = temp_path.ok_or_else(|| EditFileError::Io {
        path: requested.to_path_buf(),
        message: "could not allocate a temporary file".into(),
    })?;
    let mut cleanup = TempFileGuard::new(temp_path.clone());
    let mut file = created_file.expect("temporary path and file are created together");
    let write_result = async {
        file.write_all(&replacement)
            .await
            .map_err(|error| map_io(requested, error))?;
        file.flush()
            .await
            .map_err(|error| map_io(requested, error))?;
        file.sync_all()
            .await
            .map_err(|error| map_io(requested, error))?;
        drop(file);
        tokio::fs::set_permissions(&temp_path, metadata.permissions())
            .await
            .map_err(|error| map_io(requested, error))?;
        // Guard against a late change during the temp-file write.
        let latest_path = resolve(root, requested).await?;
        if latest_path != path {
            return Err(EditFileError::OutsideWorkspace {
                path: requested.to_path_buf(),
            });
        }
        let (_, latest) = read_workspace_file(root, &path, requested).await?;
        if latest.len() as u64 > MAX_EDIT_FILE_BYTES {
            return Err(EditFileError::TooLarge {
                field: "file".into(),
                max_bytes: usize::try_from(MAX_EDIT_FILE_BYTES).unwrap_or(usize::MAX),
            });
        }
        let latest_hash = hash(&latest);
        if latest_hash != old_hash {
            return Err(EditFileError::Conflict {
                expected: old_hash.clone(),
                actual: latest_hash,
            });
        }
        if cancel.is_cancelled() {
            return Err(EditFileError::Cancelled);
        }
        replace_file(root, &temp_path, &path)
            .await
            .map_err(|error| map_io(requested, error))?;
        Ok::<(), EditFileError>(())
    }
    .await;
    if write_result.is_ok() {
        cleanup.commit();
    }
    write_result?;

    Ok(EditFileResult {
        old_hash,
        new_hash,
        replacements: 1,
    })
}

async fn read_workspace_file(
    root: &Path,
    path: &Path,
    requested: &Path,
) -> Result<(std::fs::Metadata, Vec<u8>), EditFileError> {
    let path_metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| map_io(requested, error))?;
    if !path_metadata.is_file() {
        return Err(EditFileError::NotRegularFile {
            path: requested.to_path_buf(),
        });
    }
    let file = open_workspace_file(root, path)
        .await
        .map_err(|error| map_io(requested, error))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| map_io(requested, error))?;
    if !metadata.is_file() {
        return Err(EditFileError::NotRegularFile {
            path: requested.to_path_buf(),
        });
    }
    let mut bytes = Vec::new();
    let read_limit = MAX_EDIT_FILE_BYTES.saturating_add(1);
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| map_io(requested, error))?;
    if metadata.len() > MAX_EDIT_FILE_BYTES || bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(EditFileError::TooLarge {
            field: "file".into(),
            max_bytes: usize::try_from(MAX_EDIT_FILE_BYTES).unwrap_or(usize::MAX),
        });
    }
    Ok((metadata, bytes))
}

async fn resolve(root: &Path, requested: &Path) -> Result<PathBuf, EditFileError> {
    let root =
        tokio::fs::canonicalize(root)
            .await
            .map_err(|error| EditFileError::WorkspaceRoot {
                message: error.to_string(),
            })?;
    if !tokio::fs::metadata(&root)
        .await
        .map_err(|error| EditFileError::WorkspaceRoot {
            message: error.to_string(),
        })?
        .is_dir()
    {
        return Err(EditFileError::WorkspaceRoot {
            message: "not a directory".into(),
        });
    }
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let lexical = normalize(&candidate);
    if !lexical.starts_with(&root) {
        return Err(EditFileError::OutsideWorkspace {
            path: requested.to_path_buf(),
        });
    }
    let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EditFileError::NotFound {
                path: requested.to_path_buf(),
            }
        } else {
            map_io(requested, error)
        }
    })?;
    if !canonical.starts_with(&root) {
        return Err(EditFileError::OutsideWorkspace {
            path: requested.to_path_buf(),
        });
    }
    Ok(canonical)
}

async fn acquire_edit_lock(
    path: &Path,
    requested: &Path,
    cancel: CancellationToken,
) -> Result<Arc<File>, EditFileError> {
    let directory = std::env::temp_dir().join("mini-harness-edit-locks");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| EditFileError::Io {
            path: requested.to_path_buf(),
            message: format!("failed to create edit lock directory: {error}"),
        })?;
    let lock_path = directory.join(format!("{}.lock", hash(path.to_string_lossy().as_bytes())));
    let file = tokio::task::spawn_blocking(move || {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
    })
    .await
    .map_err(|error| EditFileError::Io {
        path: requested.to_path_buf(),
        message: format!("failed to open edit lock: {error}"),
    })?
    .map_err(|error| EditFileError::Io {
        path: requested.to_path_buf(),
        message: format!("failed to open edit lock: {error}"),
    })?;
    let file = Arc::new(file);
    loop {
        let candidate = Arc::clone(&file);
        let result = tokio::task::spawn_blocking(move || candidate.try_lock_exclusive())
            .await
            .map_err(|error| EditFileError::Io {
                path: requested.to_path_buf(),
                message: format!("failed to acquire edit lock: {error}"),
            })?;
        match result {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(EditFileError::Cancelled),
                    _ = sleep(Duration::from_millis(5)) => {}
                }
            }
            Err(error) => {
                return Err(EditFileError::Io {
                    path: requested.to_path_buf(),
                    message: format!("failed to acquire edit lock: {error}"),
                });
            }
        }
    }
}

fn find_occurrences(text: &str, needle: &str) -> (usize, Option<usize>) {
    if needle.is_empty() {
        return (0, None);
    }
    let mut count = 0;
    let mut first = None;
    let mut offset = 0;
    while offset < text.len() {
        let Some(relative) = text[offset..].find(needle) else {
            break;
        };
        let start = offset + relative;
        count += 1;
        first.get_or_insert(start);
        let step = text[start..]
            .chars()
            .next()
            .expect("match starts at a character boundary")
            .len_utf8();
        offset = start + step;
    }
    (count, first)
}

struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn commit(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let _ = std::fs::remove_file(path);
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn map_io(path: &Path, error: std::io::Error) -> EditFileError {
    #[cfg(unix)]
    if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::EXDEV)) {
        return EditFileError::OutsideWorkspace {
            path: path.to_path_buf(),
        };
    }
    match error.kind() {
        std::io::ErrorKind::NotFound => EditFileError::NotFound {
            path: path.to_path_buf(),
        },
        std::io::ErrorKind::PermissionDenied => EditFileError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => EditFileError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        },
    }
}
