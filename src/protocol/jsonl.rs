use super::event_projection::project_event;
use super::jsonl_transport::{BoundedLine, read_bounded_line, write_json_line};
use crate::{
    config::{ConfiguredPolicy, HarnessConfig},
    durable::{Event, EventPayload, EventStore, JsonlEventStore},
    executor::LocalExecutor,
    model::ConfiguredProvider,
    policy::ToolPolicy,
    runtime::{EventSeq, SessionId, SessionState, UserInput, session::SessionHandle},
    tools::{BashTool, EditTool, ReadTool, ToolRegistry},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};
use tokio::{
    io::{BufReader, BufWriter, Stdout},
    sync::Mutex,
    task::JoinHandle,
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BATCH: usize = 128;

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: Option<String>,
    /// Optional protocol revision negotiated independently from JSON-RPC.
    /// Revision 1 keeps the original response/event shapes; revision 2 is
    /// the notification-first protocol used by current clients.
    #[serde(default, alias = "protocolVersion")]
    protocol_version: Option<u32>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ProtocolError>,
}

#[derive(Debug, Serialize)]
struct ProtocolError {
    code: &'static str,
    message: String,
}

#[derive(Default)]
struct EventCursor {
    store: Option<Arc<JsonlEventStore>>,
    next_event: EventSeq,
    legacy_payload: bool,
    /// Shadow reducer state advanced by every event the pump observes. It
    /// lets the server checkpoint at turn boundaries without replaying the
    /// whole log each time.
    shadow_state: Option<SessionState>,
    checkpoint_path: Option<PathBuf>,
    last_checkpoint_seq: Option<EventSeq>,
    checkpoint_every_events: u64,
    /// Sticky flag set when a checkpoint became due but no actor handle was
    /// available to execute it (for example the request loop's post-`wait`
    /// flush consumed the terminal event). The pump retries on later cycles.
    checkpoint_pending: bool,
}

struct ServerState {
    /// Shared with the event pump so checkpoints can be issued through the
    /// actor (single writer) while the request loop owns session lifecycle.
    handle: Arc<Mutex<Option<SessionHandle>>>,
    actor: Option<JoinHandle<()>>,
    session_id: Option<SessionId>,
    events: Arc<Mutex<EventCursor>>,
    protocol_version: u32,
    protocol_negotiated: bool,
    legacy_next_event: EventSeq,
    protocol_version_atomic: Arc<AtomicU32>,
    config: HarnessConfig,
}

impl ServerState {
    async fn active_handle(&self) -> Option<SessionHandle> {
        self.handle.lock().await.clone()
    }

    async fn set_handle(&self, handle: SessionHandle) {
        *self.handle.lock().await = Some(handle);
    }

    async fn has_handle(&self) -> bool {
        self.handle.lock().await.is_some()
    }

    async fn take_handle(&self) -> Option<SessionHandle> {
        self.handle.lock().await.take()
    }
}

type SharedStdout = Arc<Mutex<BufWriter<Stdout>>>;

#[derive(Debug, Deserialize)]
struct SessionCreateParams {
    /// Omit to use the durable layout `<durable.root>/sessions/<id>/events.jsonl`.
    #[serde(default)]
    event_log: Option<PathBuf>,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionResumeParams {
    #[serde(default)]
    event_log: Option<PathBuf>,
    session_id: SessionId,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Overrides the protocol default for replaying durable history in the
    /// resume response. This lets a revision-2 client request a one-time
    /// replay without switching the rest of the connection to legacy mode.
    #[serde(default, alias = "replay")]
    replay_events: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TurnParams {
    #[serde(alias = "input")]
    prompt: String,
    #[serde(default)]
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
struct TurnIdParams {
    turn_id: crate::runtime::TurnId,
    #[serde(default)]
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
struct WaitParams {
    turn_id: crate::runtime::TurnId,
    #[serde(default)]
    session_id: Option<SessionId>,
    /// Optional bounded wait used by interactive clients that need to process
    /// input (for example Ctrl-C) while a turn is still running. The default
    /// remains a blocking wait for compatibility with existing clients.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ApprovalParams {
    turn_id: crate::runtime::TurnId,
    call_id: crate::runtime::ToolCallId,
    approved: bool,
    #[serde(default)]
    session_id: Option<SessionId>,
}

/// Runs the newline-delimited control protocol on stdin/stdout.
///
/// The supplied configuration supplies tool policies, resource limits, the
/// durable layout, flush mode, and the checkpoint cadence (design §16).
pub async fn run_stdio(config: HarnessConfig) -> Result<(), String> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let stdout: SharedStdout = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));
    let events = Arc::new(Mutex::new(EventCursor {
        next_event: EventSeq(1),
        checkpoint_every_events: config.durable.checkpoint_every_events,
        ..EventCursor::default()
    }));
    let protocol_version_atomic = Arc::new(AtomicU32::new(2));
    let stop_events = CancellationToken::new();
    let handle_slot: Arc<Mutex<Option<SessionHandle>>> = Arc::new(Mutex::new(None));
    let event_pump = tokio::spawn(event_pump(
        Arc::clone(&events),
        Arc::clone(&stdout),
        Arc::clone(&handle_slot),
        Arc::clone(&protocol_version_atomic),
        stop_events.clone(),
    ));
    let mut state = ServerState {
        handle: handle_slot,
        actor: None,
        session_id: None,
        events,
        protocol_version: 2,
        protocol_negotiated: false,
        legacy_next_event: EventSeq(1),
        protocol_version_atomic,
        config,
    };
    loop {
        let line = match read_bounded_line(&mut reader).await? {
            Some(BoundedLine::Line(line)) => line,
            Some(BoundedLine::TooLarge) => {
                write_shared_json_line(
                    &stdout,
                    &Response {
                        jsonrpc: "2.0",
                        id: Value::Null,
                        result: None,
                        error: Some(ProtocolError {
                            code: "message_too_large",
                            message: format!(
                                "protocol message exceeds the {MAX_MESSAGE_BYTES} byte limit"
                            ),
                        }),
                    },
                )
                .await?;
                continue;
            }
            None => break,
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut flush_after_response = false;
        let response = match serde_json::from_slice::<Request>(&line) {
            Ok(request) => {
                let is_wait = request.method == "turn.wait";
                let protocol_error = negotiate_protocol(&mut state, &request);
                if protocol_error.is_none() {
                    let mut cursor = state.events.lock().await;
                    cursor.legacy_payload = state.protocol_version == 1;
                    flush_after_response =
                        request.method == "session.create" && state.protocol_version == 2;
                }
                if protocol_error.is_none()
                    && request
                        .jsonrpc
                        .as_deref()
                        .is_some_and(|version| version != "2.0")
                {
                    Some(Response {
                        jsonrpc: "2.0",
                        id: request.id.unwrap_or(Value::Null),
                        result: None,
                        error: Some(ProtocolError {
                            code: "invalid_request",
                            message: "jsonrpc must be \"2.0\"".into(),
                        }),
                    })
                } else if let Some(error) = protocol_error {
                    Some(Response {
                        jsonrpc: "2.0",
                        id: request.id.unwrap_or(Value::Null),
                        result: None,
                        error: Some(error),
                    })
                } else {
                    let response = handle_request(&mut state, request).await;
                    if is_wait && state.protocol_version == 2 {
                        flush_pending_events(&state.events, &stdout).await?;
                    }
                    response
                }
            }
            Err(error) => Some(Response {
                jsonrpc: "2.0",
                id: Value::Null,
                result: None,
                error: Some(ProtocolError {
                    code: "invalid_json",
                    message: error.to_string(),
                }),
            }),
        };
        if let Some(response) = response {
            write_shared_json_line(&stdout, &response).await?;
        }
        if flush_after_response {
            flush_pending_events(&state.events, &stdout).await?;
        }
    }
    if let Some(handle) = state.take_handle().await {
        handle.shutdown().await;
    }
    if let Some(actor) = state.actor.take() {
        actor.await.map_err(|error| error.to_string())?;
    }
    stop_events.cancel();
    event_pump.await.map_err(|error| error.to_string())??;
    Ok(())
}

async fn handle_request(state: &mut ServerState, request: Request) -> Option<Response> {
    let id = request.id;
    let result = match request.method.as_str() {
        "protocol.negotiate" => negotiate_protocol_request(state, request.params).await,
        "session.create" => create_session(state, request.params).await,
        "session.resume" => resume_session(state, request.params).await,
        "turn.start" => start_turn(state, request.params).await,
        "turn.wait" => wait_turn(state, request.params).await,
        "turn.cancel" => cancel_turn(state, request.params).await,
        "approval.respond" => respond_approval(state, request.params).await,
        "session.shutdown" => shutdown_session(state).await,
        _ => Err(ProtocolError {
            code: "method_not_found",
            message: format!("unknown method: {}", request.method),
        }),
    };
    id.map(|id| match result {
        Ok(value) => Response {
            jsonrpc: "2.0",
            id,
            result: Some(value),
            error: None,
        },
        Err(error) => Response {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        },
    })
}

fn negotiate_protocol(state: &mut ServerState, request: &Request) -> Option<ProtocolError> {
    let requested = request.protocol_version.or_else(|| {
        // The first protocol did not send a JSON-RPC marker. Treat that wire
        // shape as revision 1 so old clients retain response and replay
        // semantics without a separate migration flag.
        (!state.protocol_negotiated && request.jsonrpc.is_none()).then_some(1)
    });
    let Some(version) = requested else {
        // A JSON-RPC 2.0 request without an explicit protocol marker uses
        // the current revision and pins that choice for the connection.
        state.protocol_negotiated = true;
        return None;
    };
    if !matches!(version, 1 | 2) {
        return Some(ProtocolError {
            code: "unsupported_protocol",
            message: format!(
                "unsupported protocol version {version}; supported versions are 1 and 2"
            ),
        });
    }
    if state.protocol_negotiated && state.protocol_version != version {
        return Some(ProtocolError {
            code: "protocol_version_mismatch",
            message: format!(
                "protocol version {} is already active; start a new connection to use version {version}",
                state.protocol_version
            ),
        });
    }
    state.protocol_version = version;
    state.protocol_negotiated = true;
    state
        .protocol_version_atomic
        .store(version, Ordering::Release);
    None
}

async fn negotiate_protocol_request(
    state: &mut ServerState,
    params: Value,
) -> Result<Value, ProtocolError> {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default, alias = "protocolVersion", alias = "protocol_version")]
        version: Option<u32>,
    }
    let params: Params = parse_params(params)?;
    if let Some(version) = params.version {
        if let Some(error) = negotiate_protocol(
            state,
            &Request {
                jsonrpc: Some("2.0".into()),
                protocol_version: Some(version),
                id: None,
                method: "protocol.negotiate".into(),
                params: Value::Null,
            },
        ) {
            return Err(error);
        }
    }
    Ok(json!({
        "protocol_version": state.protocol_version,
        "capabilities": {
            "event_notifications": true,
            "legacy_events": true,
            "resume_replay": state.protocol_version == 1,
            "max_message_bytes": MAX_MESSAGE_BYTES,
        }
    }))
}

async fn create_session(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: SessionCreateParams = parse_params(params)?;
    if state.has_handle().await {
        return Err(ProtocolError {
            code: "session_exists",
            message: "a session is already active".into(),
        });
    }
    let session_id = SessionId::new();
    let workspace = params
        .workspace
        .clone()
        .unwrap_or(std::env::current_dir().unwrap_or_default());
    let (event_log, checkpoint_path) = match params.event_log.clone() {
        Some(event_log) => {
            let checkpoint = checkpoint_path_for_log(&event_log);
            (event_log, checkpoint)
        }
        None => (
            state.config.session_event_log(session_id),
            state.config.session_checkpoint(session_id),
        ),
    };
    let event_store =
        JsonlEventStore::with_flush_mode(event_log.clone(), state.config.durable.flush);
    event_store
        .repair_partial_tail()
        .await
        .map_err(internal_error)?;
    let (handle, actor, store) = spawn_session(
        session_id,
        event_log,
        workspace,
        params.provider,
        params.model,
        &state.config,
    )
    .await?;
    tracing::info!(
        target: "mini_harness::serve",
        session_id = %session_id,
        "session created"
    );
    state.set_handle(handle).await;
    state.actor = Some(actor);
    state.session_id = Some(session_id);
    {
        let mut cursor = state.events.lock().await;
        cursor.store = Some(store);
        cursor.next_event = EventSeq(1);
        cursor.shadow_state = Some(SessionState::new(session_id));
        cursor.checkpoint_path = Some(checkpoint_path);
        cursor.last_checkpoint_seq = None;
    }
    state.legacy_next_event = EventSeq(1);
    Ok(json!({"session_id": session_id}))
}

async fn resume_session(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: SessionResumeParams = parse_params(params)?;
    if state.has_handle().await {
        return Err(ProtocolError {
            code: "session_exists",
            message: "a session is already active".into(),
        });
    }
    let workspace = params
        .workspace
        .clone()
        .unwrap_or(std::env::current_dir().unwrap_or_default());
    let (event_log, checkpoint_path) = match params.event_log.clone() {
        Some(event_log) => {
            let checkpoint = checkpoint_path_for_log(&event_log);
            (event_log, checkpoint)
        }
        None => (
            state.config.session_event_log(params.session_id),
            state.config.session_checkpoint(params.session_id),
        ),
    };
    let event_store =
        JsonlEventStore::with_flush_mode(event_log.clone(), state.config.durable.flush);
    event_store
        .repair_partial_tail()
        .await
        .map_err(internal_error)?;
    let (handle, actor, store) = resume_session_actor(
        params.session_id,
        event_log,
        workspace,
        params.provider,
        params.model,
        &state.config,
        checkpoint_path.clone(),
    )
    .await?;
    tracing::info!(
        target: "mini_harness::serve",
        session_id = %params.session_id,
        "session resumed"
    );
    let replay_history = params.replay_events.unwrap_or(state.protocol_version == 1);
    let replayed_events = if replay_history {
        read_response_events_from(&store, EventSeq(1), state.protocol_version == 1).await?
    } else {
        Vec::new()
    };
    let next_event = next_event_seq(&store).await?;
    // Prime the shadow reducer with the full replayed prefix so the event
    // pump can checkpoint without re-reading the log.
    let mut shadow_state = SessionState::new(params.session_id);
    {
        let events = store.read_from(EventSeq(1)).await.map_err(internal_error)?;
        for event in &events {
            crate::durable::reduce(&mut shadow_state, event).map_err(internal_error)?;
        }
    }
    state.set_handle(handle).await;
    state.actor = Some(actor);
    state.session_id = Some(params.session_id);
    {
        let mut cursor = state.events.lock().await;
        cursor.store = Some(store);
        cursor.next_event = next_event;
        cursor.shadow_state = Some(shadow_state);
        cursor.checkpoint_path = Some(checkpoint_path);
        cursor.last_checkpoint_seq = Some(EventSeq(next_event.0.saturating_sub(1)));
    }
    state.legacy_next_event = next_event;
    if replay_history {
        Ok(json!({"session_id": params.session_id, "events": replayed_events}))
    } else {
        Ok(json!({"session_id": params.session_id}))
    }
}

/// Checkpoint location convention shared by the CLI (`<log>.checkpoint.json`).
fn checkpoint_path_for_log(event_log: &std::path::Path) -> PathBuf {
    event_log.with_extension("checkpoint.json")
}

async fn start_turn(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: TurnParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let handle = state.active_handle().await.ok_or_else(no_session)?;
    let (turn_id, queued) = handle
        .start_turn_with_status(UserInput(params.prompt))
        .await
        .map_err(|error| ProtocolError {
            code: match &error {
                crate::error::HarnessError::QueueLimitExceeded => "queue_full",
                _ => "turn_start_failed",
            },
            message: error.to_string(),
        })?;
    Ok(json!({"turn_id": turn_id, "accepted": true, "queued": queued}))
}

async fn wait_turn(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: WaitParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let handle = state.active_handle().await.ok_or_else(no_session)?;
    let result = if let Some(timeout_ms) = params.timeout_ms {
        match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            handle.wait_turn(params.turn_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                return Err(ProtocolError {
                    code: "turn_pending",
                    message: format!("turn {} is still running", params.turn_id),
                });
            }
        }
    } else {
        handle.wait_turn(params.turn_id).await
    };
    match result {
        Ok(text) => {
            // Keep the response field for clients that do not consume
            // notifications. Version 1 receives the original full events;
            // version 2 receives the same safe projection used by event
            // notifications.
            let events = read_response_events(state).await?;
            Ok(json!({
                "turn_id": params.turn_id,
                "text": text,
                "completed": true,
                "events": events,
            }))
        }
        Err(error) => Err(ProtocolError {
            code: if matches!(
                error.as_ref(),
                crate::error::HarnessError::ApprovalPending(_)
            ) {
                "approval_pending"
            } else if matches!(error.as_ref(), crate::error::HarnessError::Cancelled) {
                "turn_cancelled"
            } else {
                "turn_failed"
            },
            message: error.to_string(),
        }),
    }
}

async fn respond_approval(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: ApprovalParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let handle = state.active_handle().await.ok_or_else(no_session)?;
    let turn_id = params.turn_id;
    let call_id = params.call_id;
    let approved = params.approved;
    match handle.queue_approval(turn_id, call_id, approved).await {
        Ok((_, text)) => Ok(json!({
            "turn_id": turn_id,
            "call_id": call_id,
            "approved": approved,
            "accepted": true,
            "persisted": true,
            "completed": true,
            "text": text,
        })),
        Err(crate::error::HarnessError::Policy(error)) if !approved => Ok(json!({
            "turn_id": turn_id,
            "call_id": call_id,
            "approved": false,
            "accepted": true,
            "persisted": true,
            "completed": false,
            "outcome": "denied",
            "message": error.to_string(),
        })),
        Err(error) => Err(approval_error(error, turn_id, call_id)),
    }
}

fn approval_error(
    error: crate::error::HarnessError,
    turn_id: crate::runtime::TurnId,
    call_id: crate::runtime::ToolCallId,
) -> ProtocolError {
    let code = match &error {
        crate::error::HarnessError::Policy(_) => "approval_denied",
        crate::error::HarnessError::InvariantViolation(_) => "approval_not_pending",
        crate::error::HarnessError::Durable(_) => "durable_error",
        crate::error::HarnessError::Provider(_) => "turn_failed",
        crate::error::HarnessError::Tool(_) => "turn_failed",
        crate::error::HarnessError::Execution(_) => "turn_failed",
        crate::error::HarnessError::ApprovalPending(_) => "approval_pending",
        crate::error::HarnessError::Cancelled => "turn_cancelled",
        crate::error::HarnessError::Timeout => "turn_timed_out",
        crate::error::HarnessError::QueueLimitExceeded => "queue_full",
        crate::error::HarnessError::Config(_) | crate::error::HarnessError::TurnAlreadyActive => {
            "invalid_request"
        }
    };
    ProtocolError {
        code,
        message: format!("approval for turn {turn_id}, call {call_id}: {error}"),
    }
}

async fn cancel_turn(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: TurnIdParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let handle = state.active_handle().await.ok_or_else(no_session)?;
    handle
        .cancel_turn(params.turn_id)
        .await
        .map_err(internal_error)?;
    Ok(json!({"turn_id": params.turn_id, "cancelled": true}))
}

async fn shutdown_session(state: &mut ServerState) -> Result<Value, ProtocolError> {
    let result = if let Some(handle) = state.take_handle().await {
        handle.shutdown_with_result().await.map_err(internal_error)
    } else {
        Ok(())
    };
    if let Some(actor) = state.actor.take() {
        actor.await.map_err(internal_error)?;
    }
    {
        let mut cursor = state.events.lock().await;
        cursor.store = None;
        cursor.shadow_state = None;
        cursor.checkpoint_path = None;
    }
    state.session_id = None;
    result?;
    Ok(json!({"shutdown": true}))
}

async fn spawn_session(
    session_id: SessionId,
    path: PathBuf,
    workspace: PathBuf,
    provider: Option<String>,
    model: Option<String>,
    config: &HarnessConfig,
) -> Result<(SessionHandle, JoinHandle<()>, Arc<JsonlEventStore>), ProtocolError> {
    let store = Arc::new(JsonlEventStore::with_flush_mode(path, config.durable.flush));
    if store.last_seq().await.map_err(internal_error)? != EventSeq(0) {
        return Err(ProtocolError {
            code: "event_log_not_empty",
            message: "session.create requires a new event log; use session.resume".into(),
        });
    }
    store
        .append(Event::new(session_id, None, EventPayload::SessionCreated))
        .await
        .map_err(internal_error)?;
    let registry = registry_from(config);
    // Session params override the [model] config section, which overrides
    // the built-in mock default.
    let provider_name = provider
        .clone()
        .unwrap_or_else(|| config.model.provider.clone());
    let model_name = model.clone().or_else(|| config.model.name.clone());
    let provider = configured_provider(Some(provider_name.as_str()), model_name.as_deref())?;
    let (handle, actor) = crate::runtime::session::resume_with_policy_id_and_config(
        session_id,
        Arc::new(provider),
        Arc::new(
            LocalExecutor::new(workspace)
                .with_process_limits(config.execution.max_concurrent_processes),
        ),
        registry,
        Arc::clone(&store),
        policy_from(config),
        config.agent_loop_config(),
    )
    .await
    .map_err(internal_error)?;
    Ok((handle, actor, store))
}

/// Builds the standard read/edit/bash registry from configuration.
fn registry_from(config: &HarnessConfig) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::default();
    let _ = registry.register(ReadTool {
        max_bytes: config.execution.max_output_bytes,
    });
    let _ = registry.register(EditTool);
    let _ = registry.register(BashTool::with_default_timeout(
        std::time::Duration::from_millis(config.execution.default_timeout_ms),
    ));
    Arc::new(registry)
}

fn policy_from(config: &HarnessConfig) -> Arc<dyn ToolPolicy> {
    Arc::new(ConfiguredPolicy::new(config.permissions.clone()))
}

#[allow(clippy::too_many_arguments)]
async fn resume_session_actor(
    session_id: SessionId,
    path: PathBuf,
    workspace: PathBuf,
    provider: Option<String>,
    model: Option<String>,
    config: &HarnessConfig,
    checkpoint_path: PathBuf,
) -> Result<(SessionHandle, JoinHandle<()>, Arc<JsonlEventStore>), ProtocolError> {
    let store = Arc::new(JsonlEventStore::with_flush_mode(path, config.durable.flush));
    let registry = registry_from(config);
    let provider_name = provider
        .clone()
        .unwrap_or_else(|| config.model.provider.clone());
    let model_name = model.clone().or_else(|| config.model.name.clone());
    let provider = configured_provider(Some(provider_name.as_str()), model_name.as_deref())?;
    let (handle, actor) = crate::runtime::session::resume_with_policy_id_checkpoint_and_config(
        session_id,
        Arc::new(provider),
        Arc::new(
            LocalExecutor::new(workspace)
                .with_process_limits(config.execution.max_concurrent_processes),
        ),
        registry,
        Arc::clone(&store),
        checkpoint_path,
        policy_from(config),
        config.agent_loop_config(),
    )
    .await
    .map_err(internal_error)?;
    Ok((handle, actor, store))
}

fn configured_provider(
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<ConfiguredProvider, ProtocolError> {
    match provider.unwrap_or("mock") {
        "mock" => Ok(ConfiguredProvider::mock()),
        "openai" => {
            let model = model.ok_or_else(|| ProtocolError {
                code: "invalid_params",
                message: "model is required when provider is openai".into(),
            })?;
            ConfiguredProvider::openai(model).map_err(internal_error)
        }
        // DeepSeek speaks the chat-completions wire format; deepseek-chat is
        // the function-calling model and the sensible default.
        "deepseek" => {
            let model = model.unwrap_or("deepseek-chat");
            ConfiguredProvider::deepseek(model).map_err(internal_error)
        }
        other => Err(ProtocolError {
            code: "invalid_params",
            message: format!("unsupported provider `{other}`"),
        }),
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(params).map_err(|error| ProtocolError {
        code: "invalid_params",
        message: error.to_string(),
    })
}

fn no_session() -> ProtocolError {
    ProtocolError {
        code: "no_session",
        message: "create or resume a session first".into(),
    }
}

fn ensure_session_id(
    state: &ServerState,
    requested: Option<SessionId>,
) -> Result<(), ProtocolError> {
    if let Some(requested) = requested {
        if state.session_id != Some(requested) {
            return Err(ProtocolError {
                code: "session_mismatch",
                message: "request session_id does not match the active session".into(),
            });
        }
    }
    Ok(())
}

fn internal_error(error: impl ToString) -> ProtocolError {
    ProtocolError {
        code: "internal_error",
        message: error.to_string(),
    }
}

async fn next_event_seq(store: &JsonlEventStore) -> Result<EventSeq, ProtocolError> {
    let last = store.last_seq().await.map_err(internal_error)?;
    last.0
        .checked_add(1)
        .map(EventSeq)
        .ok_or_else(|| internal_error("event sequence overflow"))
}

/// Pushes durable events while a `turn.wait` request is blocked on the actor.
///
/// The request loop and this task share one writer lock, so each JSONL
/// notification is written atomically with respect to responses.
/// Pushes durable events as they appear and maintains periodic checkpoints.
///
/// The request loop and this task share one writer lock, so each JSONL
/// notification is written atomically with respect to responses. Version 2
/// connections receive notifications continuously — not only while a
/// `turn.wait` request is blocked — so the protocol behaves as a background
/// event subscription for any client that keeps reading stdout.
async fn event_pump(
    cursor: Arc<Mutex<EventCursor>>,
    stdout: SharedStdout,
    handle: Arc<Mutex<Option<SessionHandle>>>,
    protocol_version: Arc<AtomicU32>,
    stop: CancellationToken,
) -> Result<(), String> {
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = sleep(Duration::from_millis(50)) => {}
        }
        let notify = protocol_version.load(Ordering::Acquire) == 2;
        let handle = handle.lock().await.clone();
        advance_cursor(&cursor, Some(&stdout), handle, notify).await?;
    }
    Ok(())
}

/// Terminal turn events; each one is a safe point for a checkpoint.
const TURN_TERMINAL_KINDS: [&str; 4] = [
    "turn.completed",
    "turn.failed",
    "turn.cancelled",
    "turn.timed_out",
];

/// Reads new durable events, advances the shadow reducer state, optionally
/// writes notifications, and checkpoints at turn boundaries or every
/// `checkpoint_every_events` events (design §16 `[durable]`).
async fn advance_cursor(
    cursor: &Arc<Mutex<EventCursor>>,
    stdout: Option<&SharedStdout>,
    handle: Option<SessionHandle>,
    notify: bool,
) -> Result<(), String> {
    let values = {
        let mut cursor = cursor.lock().await;
        let Some(store) = cursor.store.clone() else {
            return Ok(());
        };
        let events = store
            .read_from(cursor.next_event)
            .await
            .map_err(|error| error.to_string())?;
        if events.is_empty() && !cursor.checkpoint_pending {
            return Ok(());
        }
        let every = cursor.checkpoint_every_events;
        let last_checkpoint = cursor.last_checkpoint_seq.map(|seq| seq.0).unwrap_or(0);
        let mut values = Vec::with_capacity(events.len().min(MAX_EVENT_BATCH));
        let mut checkpoint_due = cursor.checkpoint_pending;
        for event in events.into_iter().take(MAX_EVENT_BATCH) {
            cursor.next_event = EventSeq(
                event
                    .seq
                    .0
                    .checked_add(1)
                    .ok_or_else(|| "event sequence overflow".to_string())?,
            );
            if let Some(state) = cursor.shadow_state.as_mut()
                && let Err(error) = crate::durable::reduce(state, &event)
            {
                // The store validates the log on read, so a shadow mismatch
                // is a harness bug; disable shadowing instead of logging the
                // same error every cycle.
                tracing::error!(
                    target: "mini_harness::serve",
                    error = %error,
                    "event pump could not advance the shadow state; checkpoints disabled"
                );
                cursor.shadow_state = None;
            }
            let kind = event.payload.kind();
            checkpoint_due |= TURN_TERMINAL_KINDS.contains(&kind)
                || (every > 0 && event.seq.0.saturating_sub(last_checkpoint) >= every);
            values.push(project_event(&event, cursor.legacy_payload));
        }
        if checkpoint_due {
            let idle = cursor
                .shadow_state
                .as_ref()
                .is_some_and(|state| state.active_turn.is_none());
            if idle
                && let (Some(path), Some(handle)) = (cursor.checkpoint_path.clone(), handle.clone())
            {
                // Checkpoints go through the actor so the marker event is
                // appended by the session's single writer; the shadow state
                // must be idle because the actor defers this command while a
                // turn runs.
                match handle.create_checkpoint(&path).await {
                    Ok(checkpoint) => {
                        cursor.last_checkpoint_seq = Some(checkpoint.last_seq);
                        cursor.checkpoint_pending = false;
                    }
                    Err(error) => {
                        // Most likely a new turn started between our read and
                        // the command; the next cycle retries once it ends.
                        cursor.checkpoint_pending = true;
                        tracing::warn!(
                            target: "mini_harness::serve",
                            error = %error,
                            "periodic checkpoint postponed"
                        );
                    }
                }
            } else {
                // Either the session is mid-turn or this flush has no actor
                // handle (the request loop's post-`wait` flush); keep the
                // decision sticky so the pump retries on a later cycle.
                cursor.checkpoint_pending = true;
            }
        }
        values
    };
    if notify {
        if let Some(stdout) = stdout {
            for event in &values {
                write_shared_json_line(stdout, event).await?;
            }
        }
    }
    Ok(())
}

async fn flush_pending_events(
    cursor: &Arc<Mutex<EventCursor>>,
    stdout: &SharedStdout,
) -> Result<(), String> {
    advance_cursor(cursor, Some(stdout), None, true).await
}

async fn write_shared_json_line(
    stdout: &SharedStdout,
    value: &impl Serialize,
) -> Result<(), String> {
    let mut writer = stdout.lock().await;
    write_json_line(&mut *writer, value).await
}

/// Reads response events using a cursor independent from notifications. This
/// keeps the legacy `events` field useful for clients that do not consume
/// asynchronous notifications while retaining a safe projection in v2.
async fn read_response_events(state: &mut ServerState) -> Result<Vec<Value>, ProtocolError> {
    let (store, next_event) = {
        let cursor = state.events.lock().await;
        (cursor.store.clone(), state.legacy_next_event)
    };
    let Some(store) = store else {
        return Ok(Vec::new());
    };
    let events = read_response_events_from(&store, next_event, state.protocol_version == 1).await?;
    if let Some(last) = events.last() {
        if let Some(seq) = last.get("seq").and_then(Value::as_u64) {
            state.legacy_next_event = EventSeq(
                seq.checked_add(1)
                    .ok_or_else(|| internal_error("event sequence overflow"))?,
            );
        }
    }
    Ok(events)
}

async fn read_response_events_from(
    store: &JsonlEventStore,
    next_event: EventSeq,
    include_legacy_payload: bool,
) -> Result<Vec<Value>, ProtocolError> {
    let events = store.read_from(next_event).await.map_err(internal_error)?;
    let mut values = Vec::with_capacity(events.len().min(MAX_EVENT_BATCH));
    let mut encoded_len = 2usize;
    // Leave room for the surrounding JSON-RPC response and text fields. The
    // event list is deliberately truncated before write_json_line can fail
    // the entire connection with a response larger than the wire limit.
    let budget = MAX_MESSAGE_BYTES.saturating_sub(4096);
    for event in events.into_iter().take(MAX_EVENT_BATCH) {
        let value = if include_legacy_payload {
            serde_json::to_value(event).map_err(internal_error)?
        } else {
            project_event(&event, false)
        };
        let size = serde_json::to_vec(&value)
            .map_err(internal_error)?
            .len()
            .saturating_add(1);
        if encoded_len.saturating_add(size) > budget {
            break;
        }
        encoded_len = encoded_len.saturating_add(size);
        values.push(value);
    }
    Ok(values)
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    use crate::durable::{Event, EventPayload, EventStore};
    use crate::runtime::{SessionId, UserInput};

    fn input(session: SessionId, text: &str) -> Event {
        Event::new(
            session,
            None,
            EventPayload::UserInputRecorded {
                input: UserInput(text.into()),
            },
        )
    }

    async fn seeded_store(events: usize) -> (Arc<JsonlEventStore>, SessionId, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonlEventStore::new(directory.path().join("events.jsonl")));
        let session = SessionId::new();
        store
            .append(Event::new(session, None, EventPayload::SessionCreated))
            .await
            .unwrap();
        for index in 0..events {
            store
                .append(input(session, &format!("m{index}")))
                .await
                .unwrap();
        }
        (store, session, directory)
    }

    fn cursor(
        store: Arc<JsonlEventStore>,
        session: SessionId,
        every: u64,
    ) -> Arc<Mutex<EventCursor>> {
        Arc::new(Mutex::new(EventCursor {
            store: Some(store),
            next_event: EventSeq(1),
            shadow_state: Some(SessionState::new(session)),
            checkpoint_every_events: every,
            ..EventCursor::default()
        }))
    }

    /// More events than one batch: the cursor advances in MAX_EVENT_BATCH
    /// steps and nothing is lost or duplicated.
    #[tokio::test]
    async fn large_event_backlogs_are_flushed_in_batches() {
        let (store, session, _keep) = seeded_store(300).await; // 301 events total
        let cursor = cursor(store, session, 0);
        advance_cursor(&cursor, None, None, false).await.unwrap();
        {
            let cursor = cursor.lock().await;
            assert_eq!(cursor.next_event, EventSeq(MAX_EVENT_BATCH as u64 + 1));
            assert_eq!(
                cursor.shadow_state.as_ref().unwrap().last_seq,
                EventSeq(MAX_EVENT_BATCH as u64)
            );
        }
        advance_cursor(&cursor, None, None, false).await.unwrap();
        advance_cursor(&cursor, None, None, false).await.unwrap();
        let cursor = cursor.lock().await;
        assert_eq!(cursor.next_event, EventSeq(302));
        assert_eq!(
            cursor.shadow_state.as_ref().unwrap().last_seq,
            EventSeq(301)
        );
        // All 301 events were observed exactly once: session.created adds
        // no history, so the 300 inputs are the entire history.
        assert_eq!(cursor.shadow_state.as_ref().unwrap().history.len(), 300);
        assert_eq!(
            cursor.shadow_state.as_ref().unwrap().pending_inputs.len(),
            300
        );
    }

    /// A checkpoint that comes due without an actor handle stays sticky, so
    /// the pump retries on a later cycle instead of forgetting the decision.
    #[tokio::test]
    async fn due_checkpoint_without_a_handle_stays_sticky() {
        let (store, session, _keep) = seeded_store(10).await;
        let cursor = cursor(store, session, 5);
        advance_cursor(&cursor, None, None, false).await.unwrap();
        {
            let cursor = cursor.lock().await;
            assert!(
                cursor.checkpoint_pending,
                "every=5 with 11 events must set the sticky flag"
            );
        }
        // No new events: the sticky flag alone must keep advance_cursor
        // working (no early return), and it remains pending without a handle.
        advance_cursor(&cursor, None, None, false).await.unwrap();
        assert!(cursor.lock().await.checkpoint_pending);
    }

    /// The every-N counter is measured against the last checkpoint, not the
    /// last flush, so it does not retrigger every cycle once pending.
    #[tokio::test]
    async fn pending_flag_does_not_duplicate_work_per_cycle() {
        let (store, session, _keep) = seeded_store(10).await;
        let cursor = cursor(store, session, 5);
        advance_cursor(&cursor, None, None, false).await.unwrap();
        advance_cursor(&cursor, None, None, false).await.unwrap();
        advance_cursor(&cursor, None, None, false).await.unwrap();
        // Still pending, still exactly one shadow state, no panics.
        assert!(cursor.lock().await.checkpoint_pending);
    }
}
