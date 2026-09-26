use super::{Event, EventPayload, EventStore, reduce};
use crate::{
    error::DurableError,
    runtime::{EventSeq, SessionId, SessionState},
};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::OpenOptions as StdOpenOptions,
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::io::AsyncWriteExt;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

/// A durable snapshot of reducer state. The event log remains authoritative;
/// a checkpoint only shortens replay time and can always be discarded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub last_seq: EventSeq,
    pub state: SessionState,
    /// A canonical SHA-256 digest of `state`, used to reject a tampered or
    /// partially replaced snapshot before replaying events after it.
    pub state_digest: String,
    pub created_at: DateTime<Utc>,
}

impl Checkpoint {
    pub const CURRENT_SCHEMA_VERSION: u32 = 2;

    pub fn from_state(state: &SessionState) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            session_id: state.session_id,
            last_seq: state.last_seq,
            state: state.clone(),
            state_digest: state_digest(state).expect("session state is serializable"),
            created_at: Utc::now(),
        }
    }

    pub async fn write_atomic(&self, path: impl AsRef<Path>) -> Result<(), DurableError> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let _lock = acquire_checkpoint_lock(parent, path.file_name()).await?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                DurableError::Checkpoint("checkpoint path has no valid file name".into())
            })?;
        let data = serde_json::to_vec_pretty(self)?;
        if data.len() as u64 > MAX_CHECKPOINT_BYTES {
            return Err(DurableError::Checkpoint(format!(
                "checkpoint exceeds the {MAX_CHECKPOINT_BYTES} byte limit"
            )));
        }
        let (temp_path, mut file) = loop {
            let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let temp_path = parent.join(format!(".{file_name}.{id}.tmp"));
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&temp_path).await {
                Ok(file) => break (temp_path, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(DurableError::Io(error)),
            }
        };
        let write_result = async {
            file.write_all(&data).await?;
            file.write_all(b"\n").await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            replace_path(&temp_path, path).await?;
            Ok::<(), std::io::Error>(())
        }
        .await;
        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
        }
        write_result.map_err(DurableError::Io)
    }

    pub async fn read(path: impl AsRef<Path>) -> Result<Self, DurableError> {
        let path = path.as_ref();
        let metadata = tokio::fs::metadata(path).await?;
        if metadata.len() > MAX_CHECKPOINT_BYTES {
            return Err(DurableError::Checkpoint(format!(
                "checkpoint exceeds the {MAX_CHECKPOINT_BYTES} byte limit"
            )));
        }
        let bytes = tokio::fs::read(path).await?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let schema_version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| DurableError::Checkpoint("checkpoint has no schema version".into()))?
            as u32;
        if !matches!(schema_version, 1 | Self::CURRENT_SCHEMA_VERSION) {
            return Err(DurableError::Checkpoint(format!(
                "unsupported checkpoint schema version {schema_version}"
            )));
        }
        // Schema 1 snapshots predate provider continuation metadata and the
        // per-call tool input map.  Normalize those missing fields before
        // deserializing the current state shape.  Keep the original schema
        // number in the returned value so callers can decide when to rewrite
        // the file, while validating both the old and current digest forms.
        let legacy_state = value
            .get("state")
            .cloned()
            .ok_or_else(|| DurableError::Checkpoint("checkpoint has no state".into()))?;
        if schema_version == 1 {
            normalize_legacy_state(&mut value)?;
        }
        let mut checkpoint: Self = serde_json::from_value(value)?;
        checkpoint
            .state
            .rebuild_history_bytes()
            .map_err(DurableError::Json)?;
        if checkpoint.state.session_id != checkpoint.session_id {
            return Err(DurableError::Checkpoint(
                "checkpoint session id does not match reducer state".into(),
            ));
        }
        if checkpoint.state.last_seq != checkpoint.last_seq {
            return Err(DurableError::Checkpoint(
                "checkpoint sequence does not match reducer state".into(),
            ));
        }
        let expected_digest = state_digest(&checkpoint.state)?;
        if checkpoint.state_digest.is_empty() {
            checkpoint.state_digest = expected_digest.clone();
        }
        let legacy_digest = (checkpoint.schema_version == 1)
            .then(|| legacy_state_digest(&checkpoint.state))
            .transpose()?;
        let legacy_v1_digest = (checkpoint.schema_version == 1)
            .then(|| legacy_state_digest_value(&legacy_state))
            .transpose()?;
        // Checkpoint v1 did not carry a digest at all.  In that case the
        // migration establishes the current digest so subsequent writes are
        // protected by the v2 integrity check.
        let digest_matches = checkpoint.state_digest.is_empty()
            || checkpoint.state_digest == expected_digest
            || legacy_digest.as_deref() == Some(checkpoint.state_digest.as_str())
            || legacy_v1_digest.as_deref() == Some(checkpoint.state_digest.as_str());
        if !digest_matches {
            return Err(DurableError::Checkpoint(
                "checkpoint state digest does not match reducer state".into(),
            ));
        }
        Ok(checkpoint)
    }

    /// Upgrade a schema 1 snapshot in place.
    ///
    /// Reading a legacy snapshot is intentionally side-effect free. Callers
    /// that own the checkpoint file can opt into this atomic rewrite after a
    /// successful read; the resulting file uses the current schema and the
    /// digest of the normalized state (including fields that were absent in
    /// schema 1).
    pub async fn migrate_to_current_schema(path: impl AsRef<Path>) -> Result<bool, DurableError> {
        let path = path.as_ref();
        let mut checkpoint = Self::read(path).await?;
        if checkpoint.schema_version == Self::CURRENT_SCHEMA_VERSION {
            return Ok(false);
        }
        checkpoint.schema_version = Self::CURRENT_SCHEMA_VERSION;
        checkpoint.state_digest = state_digest(&checkpoint.state)?;
        checkpoint.write_atomic(path).await?;
        Ok(true)
    }
}

fn state_digest(state: &SessionState) -> Result<String, DurableError> {
    let value = serde_json::to_value(state)?;
    digest_value(&value)
}

fn legacy_state_digest(state: &SessionState) -> Result<String, DurableError> {
    let mut value = serde_json::to_value(state)?;
    if let serde_json::Value::Object(fields) = &mut value {
        fields.remove("provider_continuation");
    }
    digest_value(&value)
}

fn legacy_state_digest_value(value: &serde_json::Value) -> Result<String, DurableError> {
    let mut value = value.clone();
    if let serde_json::Value::Object(fields) = &mut value {
        fields.remove("provider_continuation");
    }
    digest_value(&value)
}

fn normalize_legacy_state(value: &mut serde_json::Value) -> Result<(), DurableError> {
    let state = value
        .get_mut("state")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| DurableError::Checkpoint("checkpoint state is not an object".into()))?;
    state
        .entry("provider_continuation")
        .or_insert(serde_json::Value::Null);
    if let Some(active_turn) = state
        .get_mut("active_turn")
        .and_then(serde_json::Value::as_object_mut)
    {
        active_turn
            .entry("tool_inputs")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
    }
    // V1 predates state_digest entirely.  serde's default keeps the field
    // optional only for this migration pass; current snapshots always carry
    // the digest generated by `Checkpoint::from_state`.
    value
        .as_object_mut()
        .expect("checkpoint was checked as an object")
        .entry("state_digest")
        .or_insert_with(|| serde_json::Value::String(String::new()));
    Ok(())
}

fn digest_value(value: &serde_json::Value) -> Result<String, DurableError> {
    let mut canonical = Vec::new();
    write_canonical_json(value, &mut canonical)?;
    let digest = Sha256::digest(canonical);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_canonical_json(
    value: &serde_json::Value,
    output: &mut Vec<u8>,
) -> Result<(), serde_json::Error> {
    match value {
        serde_json::Value::Null => output.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => serde_json::to_writer(&mut *output, value)?,
        serde_json::Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                write_canonical_json(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

async fn acquire_checkpoint_lock(
    parent: &Path,
    file_name: Option<&std::ffi::OsStr>,
) -> Result<std::fs::File, DurableError> {
    let file_name = file_name
        .ok_or_else(|| DurableError::Checkpoint("checkpoint path has no valid file name".into()))?;
    let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));
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
        file.lock_exclusive()?;
        Ok::<_, io::Error>(file)
    })
    .await
    .map_err(|error| DurableError::Io(io::Error::other(error)))?
    .map_err(DurableError::Io)
}

async fn replace_path(temp_path: &Path, path: &Path) -> Result<(), io::Error> {
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

/// Replays only events after a validated checkpoint.
pub async fn replay_from_checkpoint<S: EventStore + ?Sized>(
    checkpoint_path: impl AsRef<Path>,
    store: &S,
) -> Result<SessionState, DurableError> {
    let checkpoint = Checkpoint::read(checkpoint_path).await?;
    let store_last_seq = store.last_seq().await?;
    if store_last_seq < checkpoint.last_seq {
        return Err(DurableError::Checkpoint(format!(
            "checkpoint sequence {} is ahead of event store sequence {}",
            checkpoint.last_seq, store_last_seq
        )));
    }
    let all_events = store.read_from(EventSeq(1)).await?;
    let checkpoint_index = all_events
        .iter()
        .position(|event| event.seq == checkpoint.last_seq)
        .ok_or_else(|| {
            DurableError::Checkpoint(format!(
                "event store does not contain checkpoint sequence {}",
                checkpoint.last_seq
            ))
        })?;
    let checkpoint_event = &all_events[checkpoint_index];
    if checkpoint_event.session_id != checkpoint.session_id {
        return Err(DurableError::Checkpoint(
            "checkpoint event belongs to another session".into(),
        ));
    }
    if checkpoint_event.turn_id.is_some()
        || !matches!(&checkpoint_event.payload, EventPayload::CheckpointCreated)
    {
        return Err(DurableError::Checkpoint(
            "checkpoint sequence does not identify a checkpoint event".into(),
        ));
    }
    let mut replayed = SessionState::new(checkpoint.session_id);
    for event in all_events.iter().take(checkpoint_index + 1) {
        reduce(&mut replayed, event)
            .map_err(|error| DurableError::Checkpoint(error.to_string()))?;
    }
    if replayed != checkpoint.state {
        return Err(DurableError::Checkpoint(
            "checkpoint state does not match the event log prefix".into(),
        ));
    }
    let events = &all_events[checkpoint_index..];
    let Some(checkpoint_event) = events.first() else {
        return Err(DurableError::Checkpoint(format!(
            "event store does not contain checkpoint sequence {}",
            checkpoint.last_seq
        )));
    };
    if checkpoint_event.seq != checkpoint.last_seq {
        return Err(DurableError::Checkpoint(format!(
            "expected checkpoint sequence {}, got {}",
            checkpoint.last_seq, checkpoint_event.seq
        )));
    }
    let mut state = checkpoint.state;
    for event in events.iter().skip(1) {
        reduce(&mut state, event).map_err(|error| DurableError::Checkpoint(error.to_string()))?;
    }
    Ok(state)
}

/// Replays from a checkpoint when possible and falls back to the full log if
/// the snapshot is missing, corrupt, or inconsistent with the log.
pub async fn replay_with_checkpoint_fallback<S: EventStore + ?Sized>(
    checkpoint_path: impl AsRef<Path>,
    store: &S,
) -> Result<SessionState, DurableError> {
    let checkpoint_error = match replay_from_checkpoint(checkpoint_path, store).await {
        Ok(state) => return Ok(state),
        Err(error) => error,
    };
    let events = store.read_from(EventSeq(1)).await?;
    let Some(first_event) = events.first() else {
        return Err(checkpoint_error);
    };
    let mut state = SessionState::new(first_event.session_id);
    for event in events {
        reduce(&mut state, &event).map_err(|error| DurableError::Checkpoint(error.to_string()))?;
    }
    Ok(state)
}

/// Appends a checkpoint fact and atomically persists the resulting reducer state.
pub async fn create_checkpoint<S: EventStore + ?Sized>(
    store: &S,
    state: &SessionState,
    path: impl AsRef<Path>,
) -> Result<Checkpoint, DurableError> {
    let store_seq = store.last_seq().await?;
    let next_seq = EventSeq(
        state
            .last_seq
            .0
            .checked_add(1)
            .ok_or_else(|| DurableError::Checkpoint("event sequence overflow".into()))?,
    );
    let event = if store_seq == state.last_seq {
        store
            .append(Event::new(
                state.session_id,
                None,
                EventPayload::CheckpointCreated,
            ))
            .await?
    } else if store_seq == next_seq {
        let mut events = store.read_from(next_seq).await?;
        let Some(event) = events.pop() else {
            return Err(DurableError::Checkpoint(
                "checkpoint marker disappeared while reading the event store".into(),
            ));
        };
        if event.seq != next_seq
            || event.session_id != state.session_id
            || event.turn_id.is_some()
            || !matches!(&event.payload, EventPayload::CheckpointCreated)
        {
            return Err(DurableError::Checkpoint(
                "event after state is not a reusable checkpoint marker".into(),
            ));
        }
        event
    } else {
        return Err(DurableError::Checkpoint(format!(
            "state seq {} does not match event store seq {}",
            state.last_seq, store_seq
        )));
    };
    if event.seq != next_seq
        || event.session_id != state.session_id
        || event.turn_id.is_some()
        || !matches!(&event.payload, EventPayload::CheckpointCreated)
    {
        return Err(DurableError::Checkpoint(
            "checkpoint marker did not immediately follow the supplied state".into(),
        ));
    }
    let mut checkpoint_state = state.clone();
    reduce(&mut checkpoint_state, &event)
        .map_err(|error| DurableError::Checkpoint(error.to_string()))?;
    let checkpoint = Checkpoint::from_state(&checkpoint_state);
    checkpoint.write_atomic(path).await?;
    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        durable::{Event, EventPayload, InMemoryEventStore},
        runtime::SessionId,
    };

    #[tokio::test]
    async fn checkpoint_round_trips_and_replays_later_events() {
        let session = SessionId::new();
        let mut state = SessionState::new(session);
        let store = InMemoryEventStore::default();
        let event = Event::new(session, None, EventPayload::SessionCreated);
        crate::durable::EventStore::append(&store, event)
            .await
            .unwrap();
        state.last_seq = EventSeq(1);
        state.status = crate::runtime::SessionStatus::Idle;

        let directory = tempfile::tempdir().unwrap();
        let checkpoint_path = directory.path().join("checkpoint.json");
        let checkpoint = create_checkpoint(&store, &state, &checkpoint_path)
            .await
            .unwrap();
        assert_eq!(checkpoint.last_seq, EventSeq(2));

        let later = Event::new(
            session,
            None,
            EventPayload::UserInputRecorded {
                input: crate::runtime::UserInput("hello".into()),
            },
        );
        crate::durable::EventStore::append(&store, later)
            .await
            .unwrap();
        let restored = replay_from_checkpoint(&checkpoint_path, &store)
            .await
            .unwrap();
        assert_eq!(restored.last_seq, EventSeq(3));
        assert_eq!(restored.history.len(), 1);
    }

    #[tokio::test]
    async fn corrupt_checkpoint_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        tokio::fs::write(&path, br#"{"schema_version":99}"#)
            .await
            .unwrap();
        assert!(matches!(
            Checkpoint::read(&path).await,
            Err(DurableError::Checkpoint(_)) | Err(DurableError::Json(_))
        ));
    }

    #[tokio::test]
    async fn checkpoint_marker_can_be_reused_after_snapshot_write_failure() {
        let session = SessionId::new();
        let mut state = SessionState::new(session);
        let store = InMemoryEventStore::default();
        crate::durable::EventStore::append(
            &store,
            Event::new(session, None, EventPayload::SessionCreated),
        )
        .await
        .unwrap();
        state.last_seq = EventSeq(1);
        state.status = crate::runtime::SessionStatus::Idle;

        let directory = tempfile::tempdir().unwrap();
        let bad_path = directory.path().join("checkpoint-directory");
        tokio::fs::create_dir(&bad_path).await.unwrap();
        assert!(create_checkpoint(&store, &state, &bad_path).await.is_err());
        assert_eq!(store.last_seq().await.unwrap(), EventSeq(2));

        let good_path = directory.path().join("checkpoint.json");
        let checkpoint = create_checkpoint(&store, &state, &good_path).await.unwrap();
        assert_eq!(checkpoint.last_seq, EventSeq(2));
        assert_eq!(
            replay_from_checkpoint(&good_path, &store).await.unwrap(),
            checkpoint.state
        );
    }

    #[tokio::test]
    async fn replay_rejects_checkpoint_without_a_marker_event() {
        let session = SessionId::new();
        let mut state = SessionState::new(session);
        let store = InMemoryEventStore::default();
        crate::durable::EventStore::append(
            &store,
            Event::new(session, None, EventPayload::SessionCreated),
        )
        .await
        .unwrap();
        state.last_seq = EventSeq(1);
        state.status = crate::runtime::SessionStatus::Idle;

        let directory = tempfile::tempdir().unwrap();
        let checkpoint_path = directory.path().join("checkpoint.json");
        Checkpoint::from_state(&state)
            .write_atomic(&checkpoint_path)
            .await
            .unwrap();
        assert!(matches!(
            replay_from_checkpoint(&checkpoint_path, &store).await,
            Err(DurableError::Checkpoint(_))
        ));
    }

    #[tokio::test]
    async fn replay_falls_back_to_the_event_log_for_a_corrupt_checkpoint() {
        let session = SessionId::new();
        let store = InMemoryEventStore::default();
        crate::durable::EventStore::append(
            &store,
            Event::new(session, None, EventPayload::SessionCreated),
        )
        .await
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let checkpoint_path = directory.path().join("checkpoint.json");
        tokio::fs::write(&checkpoint_path, b"not-json")
            .await
            .unwrap();

        let state = replay_with_checkpoint_fallback(&checkpoint_path, &store)
            .await
            .unwrap();
        assert_eq!(state.session_id, session);
        assert_eq!(state.last_seq, EventSeq(1));
    }

    #[tokio::test]
    async fn checkpoint_rejects_a_tampered_state_even_when_json_is_valid() {
        let session = SessionId::new();
        let mut state = SessionState::new(session);
        let store = InMemoryEventStore::default();
        crate::durable::EventStore::append(
            &store,
            Event::new(session, None, EventPayload::SessionCreated),
        )
        .await
        .unwrap();
        state.last_seq = EventSeq(1);
        state.status = crate::runtime::SessionStatus::Idle;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        create_checkpoint(&store, &state, &path).await.unwrap();

        let mut value: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        value["state"]["history"] = serde_json::json!([{"User":"tampered"}]);
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            Checkpoint::read(&path).await,
            Err(DurableError::Checkpoint(message)) if message.contains("digest")
        ));
    }

    #[tokio::test]
    async fn checkpoint_schema_one_remains_readable() {
        let session = SessionId::new();
        let mut state = SessionState::new(session);
        let store = InMemoryEventStore::default();
        crate::durable::EventStore::append(
            &store,
            Event::new(session, None, EventPayload::SessionCreated),
        )
        .await
        .unwrap();
        state.last_seq = EventSeq(1);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        let checkpoint = create_checkpoint(&store, &state, &path).await.unwrap();
        let mut value = serde_json::to_value(&checkpoint).unwrap();
        value["schema_version"] = serde_json::json!(1);
        value["state"]
            .as_object_mut()
            .unwrap()
            .remove("provider_continuation");
        value["state_digest"] = serde_json::json!(legacy_state_digest(&checkpoint.state).unwrap());
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();
        let restored = Checkpoint::read(&path).await.unwrap();
        assert_eq!(restored.schema_version, 1);
        assert_eq!(restored.state.session_id, session);
    }

    #[tokio::test]
    async fn checkpoint_schema_one_migrates_missing_tool_inputs_and_digest() {
        let session = SessionId::new();
        let mut state = SessionState::new(session);
        state.status = crate::runtime::SessionStatus::Running;
        state.last_seq = EventSeq(3);
        state.active_turn = Some(crate::runtime::TurnState {
            turn_id: crate::runtime::TurnId::new(),
            status: crate::runtime::TurnStatus::Running,
            last_model_response: None,
            tool_calls: vec![],
            executions: Default::default(),
            tool_names: Default::default(),
            tool_inputs: Default::default(),
            approvals: Default::default(),
            final_text: None,
        });
        let checkpoint = Checkpoint::from_state(&state);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        let mut value = serde_json::to_value(&checkpoint).unwrap();
        value["schema_version"] = serde_json::json!(1);
        value["state"]
            .as_object_mut()
            .unwrap()
            .remove("provider_continuation");
        value["state"]["active_turn"]
            .as_object_mut()
            .unwrap()
            .remove("tool_inputs");
        value.as_object_mut().unwrap().remove("state_digest");
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();

        let restored = Checkpoint::read(&path).await.unwrap();
        assert_eq!(restored.schema_version, 1);
        assert!(restored.state.provider_continuation.is_none());
        assert!(restored.state.active_turn.unwrap().tool_inputs.is_empty());
        assert!(!restored.state_digest.is_empty());
    }

    #[tokio::test]
    async fn checkpoint_schema_one_can_be_upgraded_in_place() {
        let session = SessionId::new();
        let state = SessionState::new(session);
        let checkpoint = Checkpoint::from_state(&state);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        let mut value = serde_json::to_value(&checkpoint).unwrap();
        value["schema_version"] = serde_json::json!(1);
        value.as_object_mut().unwrap().remove("state_digest");
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();

        assert!(Checkpoint::migrate_to_current_schema(&path).await.unwrap());
        assert!(!Checkpoint::migrate_to_current_schema(&path).await.unwrap());
        let migrated = Checkpoint::read(&path).await.unwrap();
        assert_eq!(migrated.schema_version, Checkpoint::CURRENT_SCHEMA_VERSION);
        assert!(!migrated.state_digest.is_empty());
    }
}
