use super::r#trait::{
    EditFileRequest, EditFileResult, Executor, ProcessRequest, ProcessResult, ReadFileRequest,
    ReadFileResult,
};
use crate::{
    error::{EditFileError, ProcessError, ReadFileError},
    runtime::types::WorkspaceRoot,
};
use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

const MAX_READ_BYTES: usize = 16 * 1024 * 1024;

/// Default budget a process waits for a free concurrency slot (design §21
/// “executor 等待队列上限”).
const DEFAULT_SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct LocalExecutor {
    root: WorkspaceRoot,
    /// Global process-concurrency limiter shared by every clone of this
    /// executor (design §12 “全局 executor 并发限制”).
    process_slots: Option<Arc<tokio::sync::Semaphore>>,
    slot_wait: std::time::Duration,
}

impl LocalExecutor {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: WorkspaceRoot(root),
            process_slots: None,
            slot_wait: DEFAULT_SLOT_WAIT,
        }
    }

    /// Caps how many child processes this executor (and its clones) run at
    /// once. `max_concurrent` of zero keeps the executor unlimited.
    pub fn with_process_limits(mut self, max_concurrent: usize) -> Self {
        self.process_slots =
            (max_concurrent > 0).then(|| Arc::new(tokio::sync::Semaphore::new(max_concurrent)));
        self
    }

    /// Overrides how long a process may wait for a free slot.
    pub fn with_slot_wait(mut self, wait: std::time::Duration) -> Self {
        self.slot_wait = wait;
        self
    }

    async fn acquire_slot(
        &self,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, ProcessError> {
        let Some(slots) = &self.process_slots else {
            return Ok(None);
        };
        match tokio::time::timeout(self.slot_wait, slots.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Some(permit)),
            Ok(Err(_)) => Err(ProcessError::ConcurrencyLimit),
            Err(_) => Err(ProcessError::ConcurrencyLimit),
        }
    }

    pub fn workspace_root(&self) -> WorkspaceRoot {
        self.root.clone()
    }

    async fn resolve(&self, requested: &Path) -> Result<PathBuf, ReadFileError> {
        let root = tokio::fs::canonicalize(&self.root.0)
            .await
            .map_err(|error| ReadFileError::WorkspaceRoot {
                path: self.root.0.clone(),
                message: error.to_string(),
            })?;
        let root_metadata =
            tokio::fs::metadata(&root)
                .await
                .map_err(|error| ReadFileError::WorkspaceRoot {
                    path: self.root.0.clone(),
                    message: error.to_string(),
                })?;
        if !root_metadata.is_dir() {
            return Err(ReadFileError::WorkspaceRoot {
                path: self.root.0.clone(),
                message: "not a directory".into(),
            });
        }

        let lexical = lexical_normalize(if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            root.join(requested)
        });
        let lexical_within = lexical.starts_with(&root);

        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            root.join(requested)
        };
        let canonical = tokio::fs::canonicalize(candidate).await.map_err(|error| {
            if lexical_within {
                ReadFileError::from_io(requested.to_path_buf(), error)
            } else {
                ReadFileError::OutsideWorkspace {
                    path: requested.to_path_buf(),
                }
            }
        })?;
        if !canonical.starts_with(&root) {
            return Err(ReadFileError::OutsideWorkspace {
                path: requested.to_path_buf(),
            });
        }
        Ok(canonical)
    }
}

fn map_read_open_error(path: &Path, error: std::io::Error) -> ReadFileError {
    #[cfg(unix)]
    if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::EXDEV)) {
        return ReadFileError::OutsideWorkspace {
            path: path.to_path_buf(),
        };
    }
    ReadFileError::from_io(path.to_path_buf(), error)
}

fn lexical_normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn read_file(&self, request: ReadFileRequest) -> Result<ReadFileResult, ReadFileError> {
        let requested = Path::new(&request.path);
        let path = self.resolve(requested).await?;
        if tokio::fs::metadata(&path)
            .await
            .map_err(|error| ReadFileError::from_io(requested.to_path_buf(), error))?
            .is_dir()
        {
            return Err(ReadFileError::IsDirectory {
                path: requested.to_path_buf(),
            });
        }
        let file = super::filesystem::open_workspace_file(&self.root.0, &path)
            .await
            .map_err(|error| map_read_open_error(requested, error))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|error| ReadFileError::from_io(requested.to_path_buf(), error))?;
        if metadata.is_dir() {
            return Err(ReadFileError::IsDirectory {
                path: requested.to_path_buf(),
            });
        }
        if !metadata.is_file() {
            return Err(ReadFileError::NotRegularFile {
                path: requested.to_path_buf(),
            });
        }
        let requested_limit = request.max_bytes.0.min(MAX_READ_BYTES);
        let limit = u64::try_from(requested_limit)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| ReadFileError::from_io(requested.to_path_buf(), error))?;

        let observed_bytes = bytes.len() as u64;
        let original_bytes =
            usize::try_from(metadata.len().max(observed_bytes)).unwrap_or(usize::MAX);
        let truncated = original_bytes > requested_limit || observed_bytes > requested_limit as u64;
        let sample_was_cut =
            metadata.len() > bytes.len() as u64 || observed_bytes > requested_limit as u64;
        if bytes.contains(&0)
            || (!sample_was_cut && std::str::from_utf8(&bytes).is_err())
            || (sample_was_cut
                && std::str::from_utf8(&bytes)
                    .err()
                    .is_some_and(|error| error.error_len().is_some()))
        {
            return Err(ReadFileError::BinaryFile {
                path: requested.to_path_buf(),
            });
        }
        let mut retained_bytes = bytes.len().min(requested_limit);
        let retained = &bytes[..retained_bytes];
        let text = match std::str::from_utf8(retained) {
            Ok(text) => text.to_owned(),
            Err(error) if error.error_len().is_none() && truncated => {
                retained_bytes = error.valid_up_to();
                std::str::from_utf8(&retained[..retained_bytes])
                    .expect("valid_up_to identifies a UTF-8 boundary")
                    .to_owned()
            }
            Err(_) => {
                return Err(ReadFileError::BinaryFile {
                    path: requested.to_path_buf(),
                });
            }
        };
        Ok(ReadFileResult {
            text,
            truncated,
            original_bytes,
            retained_bytes,
        })
    }

    async fn run_process(
        &self,
        request: ProcessRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ProcessResult, ProcessError> {
        let _slot = self.acquire_slot().await?;
        super::process::run_process(&self.root.0, request, cancel).await
    }

    async fn edit_file(&self, request: EditFileRequest) -> Result<EditFileResult, EditFileError> {
        super::filesystem::edit_file(&self.root.0, request).await
    }

    async fn edit_file_with_cancel(
        &self,
        request: EditFileRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<EditFileResult, EditFileError> {
        super::filesystem::edit_file_with_cancel(&self.root.0, request, cancel).await
    }

    fn workspace_root(&self) -> Option<WorkspaceRoot> {
        Some(self.workspace_root())
    }
}
