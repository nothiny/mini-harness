use super::{
    event::{Event, EventPayload},
    reducer::reduce,
    store::EventStore,
};
use crate::{
    config::FlushMode,
    error::DurableError,
    runtime::{SessionId, SessionState, types::EventSeq},
};
use async_trait::async_trait;
use fs2::FileExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::OpenOptions as StdOpenOptions,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

const MAX_EVENT_LOG_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EVENT_COUNT: usize = 1_000_000;
static MIGRATION_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
#[derive(Clone)]
pub struct JsonlEventStore {
    path: PathBuf,
    flush: FlushMode,
    lock: Arc<tokio::sync::Mutex<()>>,
    cache: Arc<Mutex<Option<LogCache>>>,
    diagnostics: Arc<StoreDiagnostics>,
}

/// Test-visible counters for the incremental-validation regression test
/// (design §21: cross-process appends must not degrade to O(n²) replays).
#[derive(Default)]
pub struct StoreDiagnostics {
    pub(crate) full_replays: AtomicU64,
    pub(crate) suffix_validations: AtomicU64,
}

impl JsonlEventStore {
    /// Number of times the whole log was re-read and re-reduced.
    pub fn full_replays(&self) -> u64 {
        self.diagnostics.full_replays.load(Ordering::Relaxed)
    }

    /// Number of times a changed log was validated by reading only the new
    /// suffix after the previously validated byte offset.
    pub fn suffix_validations(&self) -> u64 {
        self.diagnostics.suffix_validations.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct LogCache {
    fingerprint: LogFingerprint,
    events: Vec<Event>,
    /// Byte offset just past the last validated line. A later read whose file
    /// is at least this long can validate only the appended suffix.
    validated_len: u64,
    /// Reducer state after `events`; advanced incrementally with the suffix.
    state: SessionState,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct LogFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

impl JsonlEventStore {
    pub fn new(path: PathBuf) -> Self {
        // Library default keeps the strongest durability; callers that want
        // the documented `buffered` mode opt in explicitly.
        Self::with_flush_mode(path, FlushMode::Synced)
    }

    /// Creates a store with a configured append durability (design §11.1).
    pub fn with_flush_mode(path: PathBuf, flush: FlushMode) -> Self {
        Self {
            path,
            flush,
            lock: Arc::new(tokio::sync::Mutex::new(())),
            cache: Arc::new(Mutex::new(None)),
            diagnostics: Arc::new(StoreDiagnostics::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.clone();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("log");
        path.set_extension(format!("{extension}.lock"));
        path
    }

    async fn acquire_file_lock(&self, exclusive: bool) -> Result<std::fs::File, DurableError> {
        let lock_path = self.lock_path();
        if let Some(parent) = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::task::spawn_blocking(move || {
            let mut options = StdOpenOptions::new();
            options.create(true).truncate(false).read(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(lock_path)?;
            #[cfg(unix)]
            {
                let mut permissions = file.metadata()?.permissions();
                permissions.set_mode(0o600);
                file.set_permissions(permissions)?;
            }
            if exclusive {
                FileExt::lock_exclusive(&file)?;
            } else {
                FileExt::lock_shared(&file)?;
            }
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(|error| DurableError::Io(io::Error::other(error)))?
        .map_err(DurableError::Io)
    }

    /// Removes only an incomplete final line left by a crash.
    ///
    /// A malformed complete line is never repaired automatically because it
    /// may represent lost or tampered durable state.
    pub async fn repair_partial_tail(&self) -> Result<bool, DurableError> {
        let _guard = self.lock.lock().await;
        let _file_lock = self.acquire_file_lock(true).await?;
        if !self.path.exists() {
            return Ok(false);
        }
        let metadata = tokio::fs::metadata(&self.path).await?;
        if metadata.len() > MAX_EVENT_LOG_BYTES {
            return Err(DurableError::LimitExceeded(format!(
                "event log exceeds the {MAX_EVENT_LOG_BYTES} byte limit"
            )));
        }
        let bytes = tokio::fs::read(&self.path).await?;
        if bytes.is_empty() || bytes.ends_with(b"\n") {
            return Ok(false);
        }
        let truncate_at = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let file = OpenOptions::new().write(true).open(&self.path).await?;
        file.set_len(truncate_at as u64).await?;
        file.sync_data().await?;
        *self.cache.lock().expect("event log cache lock poisoned") = None;
        Ok(true)
    }
    async fn read_all_locked(&self) -> Result<Vec<Event>, DurableError> {
        if !self.path.exists() {
            *self.cache.lock().expect("event log cache lock poisoned") = None;
            return Ok(vec![]);
        }
        let metadata = tokio::fs::metadata(&self.path).await?;
        if metadata.len() > MAX_EVENT_LOG_BYTES {
            return Err(DurableError::LimitExceeded(format!(
                "event log exceeds the {MAX_EVENT_LOG_BYTES} byte limit"
            )));
        }
        let fingerprint = LogFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
        if let Some(cache) = self
            .cache
            .lock()
            .expect("event log cache lock poisoned")
            .as_ref()
            .filter(|cache| cache.fingerprint == fingerprint)
        {
            return Ok(cache.events.clone());
        }
        // Incremental path (design §21): when another process appended to the
        // log, validate only the suffix after the previously validated byte
        // offset instead of re-reading and re-reducing the whole file. All
        // writers use the same advisory file lock and append whole lines, so
        // a longer file with a complete final line is an append-only
        // extension of the validated prefix. Anything else (shrink, rewrite,
        // partial tail) falls back to the full read below, which keeps the
        // strict corruption semantics.
        {
            let cached = self
                .cache
                .lock()
                .expect("event log cache lock poisoned")
                .clone();
            if let Some(cache) = cached
                && cache.validated_len > 0
                && metadata.len() > cache.validated_len
                && self
                    .validate_suffix(&cache, metadata.len())
                    .await?
                    .is_some()
            {
                return self.cached_events();
            }
        }
        self.diagnostics
            .full_replays
            .fetch_add(1, Ordering::Relaxed);
        let bytes = tokio::fs::read(&self.path).await?;
        if bytes.is_empty() {
            *self.cache.lock().expect("event log cache lock poisoned") = Some(LogCache {
                fingerprint,
                events: Vec::new(),
                validated_len: metadata.len(),
                state: SessionState::new(SessionId::new()),
            });
            return Ok(vec![]);
        }
        if !bytes.ends_with(b"\n") {
            return Err(DurableError::Corrupt(
                bytes.iter().filter(|b| **b == b'\n').count() + 1,
            ));
        }
        let text = String::from_utf8(bytes).map_err(|_| DurableError::Corrupt(0))?;
        let mut events = Vec::new();
        let complete_text = &text[..text.len() - 1];
        for (index, line) in complete_text.split('\n').enumerate() {
            let line_number = index + 1;
            if line_number > MAX_EVENT_COUNT {
                return Err(DurableError::LimitExceeded(format!(
                    "event log exceeds the {MAX_EVENT_COUNT} event limit"
                )));
            }
            if line.trim().is_empty() {
                return Err(DurableError::Corrupt(line_number));
            }
            let event = Event::from_json(line).map_err(|error| match error {
                DurableError::Json(_) => DurableError::Corrupt(line_number),
                other => other,
            })?;
            if event.seq.0 != events.len() as u64 + 1 {
                return Err(DurableError::Corrupt(line_number));
            }
            if events
                .first()
                .is_some_and(|first: &Event| first.session_id != event.session_id)
            {
                return Err(DurableError::Corrupt(line_number));
            }
            if events.is_empty()
                && (!matches!(&event.payload, EventPayload::SessionCreated)
                    || event.turn_id.is_some())
            {
                return Err(DurableError::Corrupt(line_number));
            }
            events.push(event);
        }
        let mut state = SessionState::new(
            events
                .first()
                .map(|event| event.session_id)
                .unwrap_or_else(SessionId::new),
        );
        for (index, event) in events.iter().enumerate() {
            reduce(&mut state, event)
                .map_err(|_| DurableError::Corrupt(index.saturating_add(1)))?;
        }
        *self.cache.lock().expect("event log cache lock poisoned") = Some(LogCache {
            fingerprint,
            events: events.clone(),
            validated_len: metadata.len(),
            state,
        });
        Ok(events)
    }

    fn cached_events(&self) -> Result<Vec<Event>, DurableError> {
        Ok(self
            .cache
            .lock()
            .expect("event log cache lock poisoned")
            .as_ref()
            .map(|cache| cache.events.clone())
            .unwrap_or_default())
    }

    /// Validates the appended suffix of a longer log against a cached prefix.
    /// Returns `Ok(Some(()))` when the cache was advanced, `Ok(None)` when the
    /// caller must fall back to a full read, and `Err` on real corruption.
    async fn validate_suffix(
        &self,
        cache: &LogCache,
        new_len: u64,
    ) -> Result<Option<()>, DurableError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(&self.path).await?;
        file.seek(std::io::SeekFrom::Start(cache.validated_len))
            .await?;
        let mut suffix = Vec::new();
        file.read_to_end(&mut suffix).await?;
        if suffix.len() as u64 != new_len - cache.validated_len {
            // The file changed while we were reading; the caller retries via
            // the full path under the same lock.
            return Ok(None);
        }
        if suffix.is_empty() {
            // Only mtime moved (for example a touch): refresh the fingerprint.
            if let Ok(metadata) = tokio::fs::metadata(&self.path).await {
                self.store_cache(|cache| {
                    cache.fingerprint = LogFingerprint {
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                    };
                });
            }
            return Ok(Some(()));
        }
        if !suffix.ends_with(b"\n") {
            // Another process crashed mid-append; the full read reports the
            // precise corrupt line (and repair_partial_tail can fix it).
            return Ok(None);
        }
        let text = String::from_utf8(suffix).map_err(|_| DurableError::Corrupt(0))?;
        let mut events = cache.events.clone();
        let mut state = cache.state.clone();
        for (index, line) in text[..text.len() - 1].split('\n').enumerate() {
            if line.trim().is_empty() {
                return Err(DurableError::Corrupt(cache.events.len() + index + 1));
            }
            if cache.events.len() + index + 1 > MAX_EVENT_COUNT {
                return Err(DurableError::LimitExceeded(format!(
                    "event log exceeds the {MAX_EVENT_COUNT} event limit"
                )));
            }
            let event = Event::from_json(line).map_err(|error| match error {
                DurableError::Json(_) => DurableError::Corrupt(cache.events.len() + index + 1),
                other => other,
            })?;
            if event.seq.0 != events.len() as u64 + 1 {
                return Err(DurableError::Corrupt(cache.events.len() + index + 1));
            }
            if events
                .first()
                .is_some_and(|first: &Event| first.session_id != event.session_id)
            {
                return Err(DurableError::Corrupt(cache.events.len() + index + 1));
            }
            reduce(&mut state, &event)
                .map_err(|_| DurableError::Corrupt(cache.events.len() + index + 1))?;
            events.push(event);
        }
        self.diagnostics
            .suffix_validations
            .fetch_add(1, Ordering::Relaxed);
        let fingerprint = LogFingerprint {
            len: new_len,
            modified: tokio::fs::metadata(&self.path)
                .await
                .ok()
                .and_then(|m| m.modified().ok()),
        };
        self.cache
            .lock()
            .expect("event log cache lock poisoned")
            .replace(LogCache {
                fingerprint,
                validated_len: new_len,
                events,
                state,
            });
        Ok(Some(()))
    }

    fn store_cache(&self, update: impl FnOnce(&mut LogCache)) {
        let mut guard = self.cache.lock().expect("event log cache lock poisoned");
        if let Some(cache) = guard.as_mut() {
            update(cache);
        }
    }

    async fn read_all(&self) -> Result<Vec<Event>, DurableError> {
        let _file_lock = self.acquire_file_lock(false).await?;
        self.read_all_locked().await
    }

    /// Rewrite a mixed schema 1/2 log as one schema 2 log.
    ///
    /// The event payloads are preserved byte-for-byte at the semantic level;
    /// only each event's schema marker is upgraded. The operation validates
    /// and replays the complete log first, then atomically replaces it while
    /// holding the same file lock used by append/read. This makes migration
    /// safe to run while another process has the store open.
    pub async fn migrate_to_current_schema(&self) -> Result<bool, DurableError> {
        let _guard = self.lock.lock().await;
        let _file_lock = self.acquire_file_lock(true).await?;
        let events = self.read_all_locked().await?;
        if events
            .iter()
            .all(|event| event.schema_version == Event::CURRENT_SCHEMA_VERSION)
        {
            return Ok(false);
        }
        let mut migrated = Vec::with_capacity(events.len());
        for event in events {
            migrated.push(event.migrate_to_current()?);
        }
        let mut data = Vec::new();
        for event in &migrated {
            let mut line = serde_json::to_vec(event)?;
            line.push(b'\n');
            data.extend_from_slice(&line);
        }
        if data.len() as u64 > MAX_EVENT_LOG_BYTES {
            return Err(DurableError::LimitExceeded(format!(
                "event log exceeds the {MAX_EVENT_LOG_BYTES} byte limit"
            )));
        }
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| DurableError::Io(io::Error::other("event log path has no file name")))?;
        let id = MIGRATION_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".{file_name}.{id}.migration.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp_path).await?;
        let write_result = async {
            file.write_all(&data).await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            replace_log_path(&temp_path, &self.path).await
        }
        .await;
        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
        }
        write_result.map_err(DurableError::Io)?;
        let metadata = tokio::fs::metadata(&self.path).await?;
        let mut state = SessionState::new(
            migrated
                .first()
                .map(|event| event.session_id)
                .unwrap_or_else(SessionId::new),
        );
        for event in &migrated {
            if reduce(&mut state, event).is_err() {
                state = SessionState::new(state.session_id);
                break;
            }
        }
        *self.cache.lock().expect("event log cache lock poisoned") = Some(LogCache {
            fingerprint: LogFingerprint {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            },
            events: migrated,
            validated_len: metadata.len(),
            state,
        });
        Ok(true)
    }
}

async fn replace_log_path(temp_path: &Path, path: &Path) -> Result<(), io::Error> {
    #[cfg(not(windows))]
    {
        tokio::fs::rename(temp_path, path).await
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let source = temp_path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let destination = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            let result = unsafe {
                windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                    source.as_ptr(),
                    destination.as_ptr(),
                    windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                        | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
                )
            };
            if result == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[async_trait]
impl EventStore for JsonlEventStore {
    async fn append(&self, mut event: Event) -> Result<Event, DurableError> {
        let _guard = self.lock.lock().await;
        let file_lock = self.acquire_file_lock(true).await?;
        let events = self.read_all_locked().await?;
        if let Some(first) = events.first() {
            if first.session_id != event.session_id {
                return Err(DurableError::Corrupt(events.len()));
            }
        }
        let cached_state = self
            .cache
            .lock()
            .expect("event log cache lock poisoned")
            .as_ref()
            .map(|cache| cache.state.clone());
        let existing = match cached_state {
            Some(state) if state.last_seq.0 == events.len() as u64 => (events, state),
            _ => {
                // No usable cached state (empty log or a dropped cache): the
                // append cache update below simply invalidates the cache.
                (events, SessionState::new(event.session_id))
            }
        };
        event.seq = EventSeq(existing.0.len() as u64 + 1);
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&self.path).await?;
        #[cfg(unix)]
        {
            let mut permissions = file.metadata().await?.permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions).await?;
        }
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        if line.len() as u64 > MAX_EVENT_LOG_BYTES {
            return Err(DurableError::LimitExceeded(
                "event exceeds the event log byte limit".into(),
            ));
        }
        let existing_bytes = tokio::fs::metadata(&self.path).await?.len();
        if existing_bytes.saturating_add(line.len() as u64) > MAX_EVENT_LOG_BYTES {
            return Err(DurableError::LimitExceeded(format!(
                "event log exceeds the {MAX_EVENT_LOG_BYTES} byte limit"
            )));
        }
        let append_result = async {
            file.write_all(&line).await?;
            file.flush().await?;
            if self.flush == FlushMode::Synced {
                file.sync_data().await?
            }
            Ok::<(), std::io::Error>(())
        }
        .await;
        drop(file);
        drop(file_lock);
        if let Err(error) = append_result {
            // A sync error is ambiguous: the kernel may already have persisted
            // the complete line. Re-read by event id before reporting failure.
            if let Ok(events) = self.read_all().await
                && let Some(persisted) = events
                    .into_iter()
                    .find(|candidate| candidate.event_id == event.event_id)
            {
                return Ok(persisted);
            }
            return Err(DurableError::Io(error));
        }
        if let Ok(metadata) = tokio::fs::metadata(&self.path).await {
            let mut cached_events = existing.0;
            let mut state = existing.1;
            // The append is already durable; if the reducer rejects it the
            // cache is dropped so the next read validates from scratch.
            match reduce(&mut state, &event) {
                Ok(()) => {
                    cached_events.push(event.clone());
                    *self.cache.lock().expect("event log cache lock poisoned") = Some(LogCache {
                        fingerprint: LogFingerprint {
                            len: metadata.len(),
                            modified: metadata.modified().ok(),
                        },
                        events: cached_events,
                        validated_len: metadata.len(),
                        state,
                    });
                }
                Err(_) => {
                    *self.cache.lock().expect("event log cache lock poisoned") = None;
                }
            }
        }
        Ok(event)
    }
    async fn read_from(&self, seq: EventSeq) -> Result<Vec<Event>, DurableError> {
        Ok(self
            .read_all()
            .await?
            .into_iter()
            .filter(|event| event.seq.0 >= seq.0)
            .collect())
    }
    async fn last_seq(&self) -> Result<EventSeq, DurableError> {
        let _file_lock = self.acquire_file_lock(false).await?;
        if !self.path.exists() {
            *self.cache.lock().expect("event log cache lock poisoned") = None;
            return Ok(EventSeq(0));
        }
        let metadata = tokio::fs::metadata(&self.path).await?;
        let fingerprint = LogFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
        if let Some(cache) = self
            .cache
            .lock()
            .expect("event log cache lock poisoned")
            .as_ref()
            .filter(|cache| cache.fingerprint == fingerprint)
        {
            return Ok(EventSeq(cache.events.len() as u64));
        }
        Ok(EventSeq(self.read_all_locked().await?.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        durable::{Event, EventPayload, EventStore},
        runtime::{SessionId, UserInput},
    };

    #[tokio::test]
    async fn reads_mixed_schema_log_and_migrates_it_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let session = SessionId::new();
        let mut first = Event::new(session, None, EventPayload::SessionCreated);
        first.seq = EventSeq(1);
        first.schema_version = 1;
        let mut second = Event::new(
            session,
            None,
            EventPayload::UserInputRecorded {
                input: UserInput("hello".into()),
            },
        );
        second.seq = EventSeq(2);
        tokio::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .await
        .unwrap();

        let store = JsonlEventStore::new(path.clone());
        let events = store.read_from(EventSeq(1)).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].schema_version, 1);
        assert_eq!(events[1].schema_version, Event::CURRENT_SCHEMA_VERSION);
        assert!(store.migrate_to_current_schema().await.unwrap());
        assert!(!store.migrate_to_current_schema().await.unwrap());
        let migrated = store.read_from(EventSeq(1)).await.unwrap();
        assert!(
            migrated
                .iter()
                .all(|event| event.schema_version == Event::CURRENT_SCHEMA_VERSION)
        );
        let raw = tokio::fs::read_to_string(path).await.unwrap();
        assert!(!raw.contains("\"schema_version\":1"));
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;
    use crate::durable::{EventPayload, EventStore};

    fn input_event(session: SessionId, text: &str) -> Event {
        Event::new(
            session,
            None,
            EventPayload::UserInputRecorded {
                input: crate::runtime::UserInput(text.into()),
            },
        )
    }

    #[tokio::test]
    async fn cross_process_appends_are_validated_by_suffix_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.jsonl");
        let session = SessionId::new();
        // "Process A" creates the log and caches a validated prefix.
        let store_a = JsonlEventStore::new(path.clone());
        store_a
            .append(Event::new(session, None, EventPayload::SessionCreated))
            .await
            .unwrap();
        for index in 0..50 {
            store_a
                .append(input_event(session, &format!("warm {index}")))
                .await
                .unwrap();
        }
        let full_replays_before = store_a.full_replays();
        assert_eq!(store_a.suffix_validations(), 0);

        // "Process B" (a second store instance) appends new events; process
        // A's next read must validate only the appended suffix instead of
        // re-reading and re-reducing the whole log.
        let store_b = JsonlEventStore::new(path.clone());
        for index in 0..10 {
            store_b
                .append(input_event(session, &format!("b {index}")))
                .await
                .unwrap();
        }
        let events = store_a.read_from(EventSeq(1)).await.unwrap();
        assert_eq!(events.len(), 61);
        assert!(matches!(
            &events.last().unwrap().payload,
            EventPayload::UserInputRecorded { input }
                if input.0 == "b 9"
        ));
        assert_eq!(store_a.suffix_validations(), 1);
        assert_eq!(store_a.full_replays(), full_replays_before);

        // Truncation (for example a repair) invalidates the prefix and falls
        // back to a full validation.
        let truncated = events[..40].to_vec();
        let mut data = Vec::new();
        for event in &truncated {
            let mut line = serde_json::to_vec(event).unwrap();
            line.push(b'\n');
            data.extend_from_slice(&line);
        }
        tokio::fs::write(&path, data).await.unwrap();
        let replays_before = store_a.full_replays();
        let events = store_a.read_from(EventSeq(1)).await.unwrap();
        assert_eq!(events.len(), 40);
        assert!(store_a.full_replays() > replays_before);
    }
}
