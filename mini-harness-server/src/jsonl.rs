//! JSONL 协议服务器: 在 stdin/stdout 上驱动 mini-harness 会话。
//!
//! 这个 crate 演示如何在简化版 [`Session`]（无 Actor）之上构建并发
//! 控制: turn 在后台任务中执行，服务器持有 `Mutex<Session>`，
//! 取消通过 [`CancellationToken`] 传递，审批在 turn 暂停
//! （`ApprovalPending`）后由请求循环直接驱动。
//!
//! 协议（v2，行分隔 JSON-RPC 2.0 风格）:
//!
//! * `session.create` / `session.resume` — 建立或恢复会话
//! * `turn.start { input }` → `{ turn_id, accepted }` — 启动 turn
//! * `turn.wait { turn_id, timeout_ms? }` → `{ text, completed }` — 等待结果
//! * `turn.cancel { turn_id }` — 取消（运行中或挂起审批中）
//! * `approval.respond { turn_id, call_id, approved }` — 审批决定
//! * `session.shutdown` — 结束会话
//! * 事件通知: `{ method: "event", params: { seq, event: {...} } }`

use crate::projection::project_event;
use crate::transport::{BoundedLine, read_bounded_line, write_json_line};
use mini_harness::{
    config::{ConfiguredPolicy, HarnessConfig},
    durable::{Event, EventPayload, EventStore, JsonlEventStore},
    executor::LocalExecutor,
    model::ConfiguredProvider,
    runtime::{EventSeq, SessionId, TurnId, session::Session},
    tools::{BashTool, EditTool, ReadTool, ToolRegistry},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::{
    io::{BufReader, BufWriter, Stdout},
    sync::{Mutex, watch},
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BATCH: usize = 128;

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: Option<String>,
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

type SharedStdout = Arc<Mutex<BufWriter<Stdout>>>;
type TurnOutcome = Arc<Result<String, mini_harness::HarnessError>>;

/// 一个活动会话: Session 本体 + 终止结果表。
struct SessionSlot {
    session: Session<ConfiguredProvider, LocalExecutor, JsonlEventStore>,
    outcomes: HashMap<TurnId, TurnOutcome>,
    /// 结果版本号: 每次写入结果时递增，waiter 通过 watch 订阅变化。
    version: watch::Sender<u64>,
}

/// 运行中 turn 的取消令牌。独立于 SessionSlot 的锁:
/// turn 在后台任务里长期持有 Session 锁，取消必须能随时读到令牌。
type RunningGate = Arc<Mutex<Option<(TurnId, CancellationToken)>>>;

struct ServerState {
    slot: Option<Arc<Mutex<SessionSlot>>>,
    running: RunningGate,
    session_id: Option<SessionId>,
    events: Arc<Mutex<EventCursor>>,
    /// Response-side cursor (the `events` field of `turn.wait` results).
    response_next_event: EventSeq,
    config: HarnessConfig,
}

#[derive(Default)]
struct EventCursor {
    store: Option<Arc<JsonlEventStore>>,
    next_event: EventSeq,
}

#[derive(Debug, Deserialize)]
struct SessionCreateParams {
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
    turn_id: TurnId,
    #[serde(default)]
    session_id: Option<SessionId>,
}

#[derive(Debug, Deserialize)]
struct WaitParams {
    turn_id: TurnId,
    #[serde(default)]
    session_id: Option<SessionId>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ApprovalParams {
    turn_id: TurnId,
    call_id: mini_harness::runtime::ToolCallId,
    approved: bool,
    #[serde(default)]
    session_id: Option<SessionId>,
}

/// Runs the newline-delimited control protocol on stdin/stdout.
pub async fn run_stdio(config: HarnessConfig) -> Result<(), String> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let stdout: SharedStdout = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));
    let events = Arc::new(Mutex::new(EventCursor {
        next_event: EventSeq(1),
        ..EventCursor::default()
    }));
    let stop = CancellationToken::new();
    let pump = tokio::spawn(event_pump(
        Arc::clone(&events),
        stdout.clone(),
        stop.clone(),
    ));
    let mut state = ServerState {
        slot: None,
        running: Arc::new(Mutex::new(None)),
        session_id: None,
        events,
        response_next_event: EventSeq(1),
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
        let response = match serde_json::from_slice::<Request>(&line) {
            Ok(request) => {
                if let Some(error) = validate_request(&request) {
                    Some(error)
                } else {
                    let response = handle_request(&mut state, request).await;
                    // 先写响应、后 flush 事件: 客户端按 id 关联响应，
                    // 事件通知作为独立流随时可读。
                    if let Some(response) = response {
                        write_shared_json_line(&stdout, &response).await?;
                    }
                    flush_pending_events(&state.events, &stdout).await?;
                    None
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
    }
    if let Some((_, cancel)) = state.running.lock().await.take() {
        cancel.cancel();
    }
    state.slot = None;
    stop.cancel();
    pump.await.map_err(|error| error.to_string())??;
    Ok(())
}

fn validate_request(request: &Request) -> Option<Response> {
    let invalid = |code: &'static str, message: String| Response {
        jsonrpc: "2.0",
        id: request.id.clone().unwrap_or(Value::Null),
        result: None,
        error: Some(ProtocolError { code, message }),
    };
    if request
        .jsonrpc
        .as_deref()
        .is_some_and(|version| version != "2.0")
    {
        return Some(invalid("invalid_request", "jsonrpc must be \"2.0\"".into()));
    }
    None
}

async fn handle_request(state: &mut ServerState, request: Request) -> Option<Response> {
    let id = request.id;
    let result = match request.method.as_str() {
        "protocol.negotiate" => Ok(json!({
            "protocol_version": 2,
            "capabilities": {
                "event_notifications": true,
                "max_message_bytes": MAX_MESSAGE_BYTES,
            }
        })),
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

async fn create_session(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: SessionCreateParams = parse_params(params)?;
    if state.slot.is_some() {
        return Err(ProtocolError {
            code: "session_exists",
            message: "a session is already active".into(),
        });
    }
    let session_id = SessionId::new();
    let workspace = params
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let event_log = params
        .event_log
        .clone()
        .unwrap_or_else(|| state.config.session_event_log(session_id));
    let store = Arc::new(JsonlEventStore::with_flush_mode(
        event_log,
        state.config.durable.flush,
    ));
    store.repair_partial_tail().await.map_err(internal_error)?;
    if store.last_seq().await.map_err(internal_error)? != EventSeq(0) {
        return Err(ProtocolError {
            code: "event_log_not_empty",
            message: "session.create requires a new event log; use session.resume".into(),
        });
    }
    // 会话在创建时即刻持久化: 客户端随后能 resume 它，即使从未发起 turn。
    store
        .append(Event::new(session_id, None, EventPayload::SessionCreated))
        .await
        .map_err(internal_error)?;
    let slot = build_slot(
        state,
        session_id,
        workspace,
        params.provider,
        params.model,
        store.clone(),
    )
    .await?;
    state.slot = Some(slot.clone());
    state.session_id = Some(session_id);
    {
        let mut cursor = state.events.lock().await;
        cursor.store = Some(store);
        cursor.next_event = EventSeq(1);
    }
    state.response_next_event = EventSeq(1);
    tracing::info!(target: "mini_harness::serve", session_id = %session_id, "session created");
    Ok(json!({"session_id": session_id}))
}

async fn resume_session(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: SessionResumeParams = parse_params(params)?;
    if state.slot.is_some() {
        return Err(ProtocolError {
            code: "session_exists",
            message: "a session is already active".into(),
        });
    }
    let workspace = params
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let event_log = params
        .event_log
        .clone()
        .unwrap_or_else(|| state.config.session_event_log(params.session_id));
    let store = Arc::new(JsonlEventStore::with_flush_mode(
        event_log,
        state.config.durable.flush,
    ));
    store.repair_partial_tail().await.map_err(internal_error)?;
    let next_event = next_event_seq(&store).await?;
    let slot = build_slot(
        state,
        params.session_id,
        workspace,
        params.provider,
        params.model,
        store.clone(),
    )
    .await?;
    state.slot = Some(slot);
    state.session_id = Some(params.session_id);
    {
        let mut cursor = state.events.lock().await;
        cursor.store = Some(store);
        cursor.next_event = next_event;
    }
    state.response_next_event = next_event;
    tracing::info!(target: "mini_harness::serve", session_id = %params.session_id, "session resumed");
    // v2 协议不重放历史: 客户端通过事件通知增量消费。
    Ok(json!({"session_id": params.session_id}))
}

async fn build_slot(
    state: &ServerState,
    session_id: SessionId,
    workspace: PathBuf,
    provider: Option<String>,
    model: Option<String>,
    store: Arc<JsonlEventStore>,
) -> Result<Arc<Mutex<SessionSlot>>, ProtocolError> {
    let provider_name = provider.unwrap_or_else(|| state.config.model.provider.clone());
    let model_name = model.or_else(|| state.config.model.name.clone());
    let provider = configured_provider(Some(provider_name.as_str()), model_name.as_deref())?;
    let session = Session::resume_with_policy(
        Arc::new(provider),
        Arc::new(
            LocalExecutor::new(workspace)
                .with_process_limits(state.config.execution.max_concurrent_processes),
        ),
        registry_from(&state.config),
        Arc::clone(&store),
        session_id,
        Arc::new(ConfiguredPolicy::new(state.config.permissions.clone())),
        state.config.agent_loop_config(),
    )
    .await
    .map_err(internal_error)?;
    let (version, _) = watch::channel(0);
    Ok(Arc::new(Mutex::new(SessionSlot {
        session,
        outcomes: HashMap::new(),
        version,
    })))
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

async fn start_turn(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: TurnParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let slot = active_slot(state)?;
    let mut slot = slot.lock().await;
    if state.running.lock().await.is_some() {
        return Err(ProtocolError {
            code: "turn_start_failed",
            message: "a turn is already running; wait for it or cancel it".into(),
        });
    }
    // begin_turn 立即持久化输入并返回 turn id；
    // 采样在后台任务中继续，事件经由 event pump 推送。
    let turn_id = slot
        .session
        .begin_turn(params.prompt)
        .await
        .map_err(turn_start_error)?;
    let cancel = CancellationToken::new();
    *state.running.lock().await = Some((turn_id, cancel.clone()));
    drop(slot);
    tokio::spawn(drive_turn(
        active_slot(state)?,
        state.running.clone(),
        turn_id,
        cancel,
    ));
    Ok(json!({"turn_id": turn_id, "accepted": true, "queued": false}))
}

/// 后台驱动一个已 begin 的 turn，并记录终止结果。
async fn drive_turn(
    slot: Arc<Mutex<SessionSlot>>,
    running: RunningGate,
    turn_id: TurnId,
    cancel: CancellationToken,
) {
    let outcome = {
        let mut slot = slot.lock().await;
        slot.session
            .continue_turn(turn_id, cancel)
            .await
            .map(|(_, text)| text)
    };
    let mut gate = running.lock().await;
    if gate.as_ref().is_some_and(|(id, _)| *id == turn_id) {
        *gate = None;
    }
    let mut slot = slot.lock().await;
    slot.outcomes.insert(turn_id, Arc::new(outcome));
    slot.version.send_modify(|version| *version += 1);
}

async fn wait_turn(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: WaitParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let slot = active_slot(state)?;
    let outcome = if let Some(timeout_ms) = params.timeout_ms {
        match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            await_turn_outcome(slot, params.turn_id),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                return Err(ProtocolError {
                    code: "turn_pending",
                    message: format!("turn {} is still running", params.turn_id),
                });
            }
        }
    } else {
        await_turn_outcome(slot, params.turn_id).await
    };
    match outcome.as_ref() {
        Ok(text) => {
            let events = read_response_events(state).await?;
            Ok(json!({
                "turn_id": params.turn_id,
                "text": text,
                "completed": true,
                "events": events,
            }))
        }
        Err(error) => Err(wait_error(error, params.turn_id)),
    }
}

/// 等待一个 turn 的终止结果（无丢失唤醒）。
///
/// 先在持锁时订阅版本号，再检查结果表: 订阅之后发生的任何结果写入
/// 都会让 `changed()` 立即返回，不会错过唯一的通知。
async fn await_turn_outcome(slot: Arc<Mutex<SessionSlot>>, turn_id: TurnId) -> TurnOutcome {
    let mut receiver = {
        let slot = slot.lock().await;
        slot.version.subscribe()
    };
    loop {
        {
            let slot = slot.lock().await;
            if let Some(outcome) = slot.outcomes.get(&turn_id) {
                return outcome.clone();
            }
        }
        if receiver.changed().await.is_err() {
            // 发送端随会话一起销毁: 不可能再有结果。
            return Arc::new(Err(mini_harness::HarnessError::InvariantViolation(
                "session dropped while waiting for a turn".into(),
            )));
        }
    }
}

fn wait_error(error: &mini_harness::HarnessError, turn_id: TurnId) -> ProtocolError {
    let code = match error {
        mini_harness::HarnessError::ApprovalPending(_) => "approval_pending",
        mini_harness::HarnessError::Cancelled => "turn_cancelled",
        mini_harness::HarnessError::Timeout => "turn_timed_out",
        _ => "turn_failed",
    };
    ProtocolError {
        code,
        message: format!("turn {turn_id}: {error}"),
    }
}

fn turn_start_error(error: mini_harness::HarnessError) -> ProtocolError {
    ProtocolError {
        code: "turn_start_failed",
        message: error.to_string(),
    }
}

async fn cancel_turn(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: TurnIdParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let slot = active_slot(state)?;
    let cancel = {
        let gate = state.running.lock().await;
        match gate.as_ref() {
            Some((id, cancel)) if *id == params.turn_id => Some(cancel.clone()),
            _ => None,
        }
    };
    if let Some(cancel) = cancel {
        // 运行中的 turn: 令牌触发循环内的取消路径（追加终止事件）。
        cancel.cancel();
        Ok(json!({"turn_id": params.turn_id, "cancelled": true}))
    } else {
        // 非运行中（例如挂起审批）: 直接写入终止事件。
        let mut slot = slot.lock().await;
        slot.session
            .cancel_turn(params.turn_id)
            .await
            .map_err(internal_error)?;
        Ok(json!({"turn_id": params.turn_id, "cancelled": true}))
    }
}

async fn respond_approval(state: &mut ServerState, params: Value) -> Result<Value, ProtocolError> {
    let params: ApprovalParams = parse_params(params)?;
    ensure_session_id(state, params.session_id)?;
    let slot = active_slot(state)?;
    let turn_id = params.turn_id;
    let call_id = params.call_id;
    let approved = params.approved;
    let mut slot = slot.lock().await;
    let result = slot.session.approve_tool(turn_id, call_id, approved).await;
    match result {
        Ok((_, ref text)) => {
            slot.outcomes.insert(turn_id, Arc::new(Ok(text.clone())));
        }
        Err(ref error) => {
            // 后续 turn.wait 复现同一终止结果（拒绝/失败/下一个审批性待）。
            let outcome = match error {
                mini_harness::HarnessError::Policy(error) => {
                    mini_harness::HarnessError::Policy(error.clone())
                }
                mini_harness::HarnessError::ApprovalPending(error) => {
                    mini_harness::HarnessError::ApprovalPending(error.clone())
                }
                mini_harness::HarnessError::Cancelled => mini_harness::HarnessError::Cancelled,
                mini_harness::HarnessError::Timeout => mini_harness::HarnessError::Timeout,
                other => mini_harness::HarnessError::InvariantViolation(other.to_string()),
            };
            slot.outcomes.insert(turn_id, Arc::new(Err(outcome)));
        }
    }
    slot.version.send_modify(|version| *version += 1);
    match result {
        Ok((_, text)) => Ok(json!({
            "turn_id": turn_id,
            "call_id": call_id,
            "approved": approved,
            "accepted": true,
            "persisted": true,
            "completed": true,
            "text": text,
        })),
        Err(mini_harness::HarnessError::Policy(error)) if !approved => Ok(json!({
            "turn_id": turn_id,
            "call_id": call_id,
            "approved": false,
            "accepted": true,
            "persisted": true,
            "completed": false,
            "outcome": "denied",
            "message": error.to_string(),
        })),
        Err(mini_harness::HarnessError::ApprovalPending(_)) => Ok(json!({
            "turn_id": turn_id,
            "call_id": call_id,
            "approved": approved,
            "accepted": true,
            "persisted": true,
            "completed": false,
            "outcome": "approval_pending",
        })),
        Err(error) => Err(approval_error(error, turn_id, call_id)),
    }
}

fn approval_error(
    error: mini_harness::HarnessError,
    turn_id: TurnId,
    call_id: mini_harness::runtime::ToolCallId,
) -> ProtocolError {
    let code = match &error {
        mini_harness::HarnessError::Policy(_) => "approval_denied",
        mini_harness::HarnessError::InvariantViolation(_) => "approval_not_pending",
        mini_harness::HarnessError::Durable(_) => "durable_error",
        mini_harness::HarnessError::Provider(_) => "turn_failed",
        mini_harness::HarnessError::Tool(_) => "turn_failed",
        mini_harness::HarnessError::Execution(_) => "turn_failed",
        mini_harness::HarnessError::ApprovalPending(_) => "approval_pending",
        mini_harness::HarnessError::Cancelled => "turn_cancelled",
        mini_harness::HarnessError::Timeout => "turn_timed_out",
        _ => "invalid_request",
    };
    ProtocolError {
        code,
        message: format!("approval for turn {turn_id}, call {call_id}: {error}"),
    }
}

async fn shutdown_session(state: &mut ServerState) -> Result<Value, ProtocolError> {
    if let Some((_, cancel)) = state.running.lock().await.take() {
        cancel.cancel();
    }
    state.slot = None;
    state.session_id = None;
    {
        let mut cursor = state.events.lock().await;
        cursor.store = None;
    }
    Ok(json!({"shutdown": true}))
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(params).map_err(|error| ProtocolError {
        code: "invalid_params",
        message: error.to_string(),
    })
}

fn active_slot(state: &ServerState) -> Result<Arc<Mutex<SessionSlot>>, ProtocolError> {
    state.slot.clone().ok_or_else(|| ProtocolError {
        code: "no_session",
        message: "create or resume a session first".into(),
    })
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

/// 后台事件泵: 轮询事件日志，把新事件投影后推送给客户端。
async fn event_pump(
    cursor: Arc<Mutex<EventCursor>>,
    stdout: SharedStdout,
    stop: CancellationToken,
) -> Result<(), String> {
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = sleep(Duration::from_millis(50)) => {}
        }
        let values = {
            let mut cursor = cursor.lock().await;
            let Some(store) = cursor.store.clone() else {
                continue;
            };
            let events = store
                .read_from(cursor.next_event)
                .await
                .map_err(|error| error.to_string())?;
            let mut values = Vec::with_capacity(events.len().min(MAX_EVENT_BATCH));
            for event in events.into_iter().take(MAX_EVENT_BATCH) {
                cursor.next_event = EventSeq(
                    event
                        .seq
                        .0
                        .checked_add(1)
                        .ok_or_else(|| "event sequence overflow".to_string())?,
                );
                values.push(project_event(&event));
            }
            values
        };
        for value in &values {
            write_shared_json_line(&stdout, value).await?;
        }
    }
    Ok(())
}

async fn flush_pending_events(
    cursor: &Arc<Mutex<EventCursor>>,
    stdout: &SharedStdout,
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
        let mut values = Vec::with_capacity(events.len().min(MAX_EVENT_BATCH));
        for event in events.into_iter().take(MAX_EVENT_BATCH) {
            cursor.next_event = EventSeq(
                event
                    .seq
                    .0
                    .checked_add(1)
                    .ok_or_else(|| "event sequence overflow".to_string())?,
            );
            values.push(project_event(&event));
        }
        values
    };
    for value in &values {
        write_shared_json_line(stdout, value).await?;
    }
    Ok(())
}

async fn write_shared_json_line(
    stdout: &SharedStdout,
    value: &impl Serialize,
) -> Result<(), String> {
    let mut writer = stdout.lock().await;
    write_json_line(&mut *writer, value).await
}

/// Reads response events using a cursor independent from notifications.
async fn read_response_events(state: &mut ServerState) -> Result<Vec<Value>, ProtocolError> {
    let store = state.events.lock().await.store.clone();
    let Some(store) = store else {
        return Ok(Vec::new());
    };
    let events = store
        .read_from(state.response_next_event)
        .await
        .map_err(internal_error)?;
    let mut values = Vec::with_capacity(events.len().min(MAX_EVENT_BATCH));
    let mut encoded_len = 2usize;
    let budget = MAX_MESSAGE_BYTES.saturating_sub(4096);
    for event in events.into_iter().take(MAX_EVENT_BATCH) {
        let value = project_event(&event);
        // 响应里的 `events` 数组携带事件本体（不带 JSON-RPC 信封）。
        let value = value["params"]["event"].clone();
        let size = serde_json::to_vec(&value)
            .map_err(internal_error)?
            .len()
            .saturating_add(1);
        if encoded_len.saturating_add(size) > budget {
            break;
        }
        encoded_len = encoded_len.saturating_add(size);
        state.response_next_event = EventSeq(event.seq.0.saturating_add(1));
        values.push(value);
    }
    Ok(values)
}
