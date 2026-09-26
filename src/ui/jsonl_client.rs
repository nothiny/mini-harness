use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc, oneshot},
};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// A split JSONL client: requests go through [`JsonlRequester`]; every
/// asynchronous message (event notifications, unmatched responses) arrives
/// on the broadcast channel. Used by the full-screen UI, which must wait on
/// stdin, server messages, and resize signals at the same time.
pub(crate) struct JsonlSplitClient {
    pub(crate) requester: JsonlRequester,
    pub(crate) messages: mpsc::Receiver<Value>,
    /// Keeps the server child alive as long as the split client lives; the
    /// reader task exits when the child closes stdout.
    pub(crate) _child: Child,
    pub(crate) _replay: Vec<Value>,
}

pub(crate) struct JsonlRequester {
    stdin: ChildStdin,
    next_id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
}

impl JsonlRequester {
    /// Sends a request and awaits exactly its response.
    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "JSONL request id overflow".to_owned())?;
        // Register the waiter before the request is written so the reader
        // task can never observe the response first.
        let (reply, result) = oneshot::channel();
        self.pending.lock().await.insert(id, reply);
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "protocol_version": 2,
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|error| format!("encode JSONL request: {error}"))?;
        line.push(b'\n');
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "JSONL request exceeds the {MAX_MESSAGE_BYTES} byte protocol limit"
            ));
        }
        self.stdin
            .write_all(&line)
            .await
            .map_err(|error| format!("write JSONL request: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("flush JSONL request: {error}"))?;
        result
            .await
            .map_err(|_| "mini-harness server exited before the response".into())
    }
}

/// Result of a `turn.wait` request made by the interactive client.
pub(crate) enum WaitOutcome {
    Completed(Value),
    Pending,
    ApprovalPending,
    Cancelled(Value),
}

pub(crate) enum WaitMode {
    Blocking,
    Poll,
}

/// Small JSONL protocol client shared by the event viewer and line-mode UI.
/// It owns only transport state; session facts remain in the Rust server.
pub(crate) struct JsonlClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    events: Vec<Value>,
}

impl JsonlClient {
    pub(crate) async fn spawn() -> Result<Self, String> {
        let mut child = Command::new(
            std::env::current_exe()
                .map_err(|error| format!("locate mini-harness executable: {error}"))?,
        )
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start mini-harness serve --stdio: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "server stdin was not piped".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "server stdout was not piped".to_owned())?;
        Ok(Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
            events: Vec::new(),
        })
    }

    /// Splits the client into a request sender plus a broadcast receiver so
    /// callers can `select!` on server messages while reading stdin.
    pub(crate) async fn into_split(self) -> Result<JsonlSplitClient, String> {
        let JsonlClient {
            child,
            stdin,
            reader,
            next_id,
            events,
        } = self;
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (broadcast, messages) = mpsc::channel(256);
        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let value = match read_line(&mut reader).await {
                    Ok(value) => value,
                    Err(_) => break,
                };
                let id = value.get("id").and_then(Value::as_u64);
                let waiter = match id {
                    Some(id) => reader_pending.lock().await.remove(&id),
                    None => None,
                };
                match waiter {
                    Some(waiter) => {
                        let _ = waiter.send(value);
                    }
                    None => {
                        if broadcast.send(value).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(JsonlSplitClient {
            requester: JsonlRequester {
                stdin,
                next_id,
                pending,
            },
            messages,
            _child: child,
            _replay: events,
        })
    }

    pub(crate) async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send(method, params).await?;
        self.read_response(id).await
    }

    pub(crate) async fn wait_turn(
        &mut self,
        session_id: &str,
        turn_id: &str,
        mode: WaitMode,
    ) -> Result<WaitOutcome, String> {
        let id = if matches!(mode, WaitMode::Poll) {
            self.send(
                "turn.wait",
                json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "timeout_ms": 100
                }),
            )
            .await?
        } else {
            self.send(
                "turn.wait",
                json!({"session_id": session_id, "turn_id": turn_id}),
            )
            .await?
        };
        let response = self.read_wire_response(id).await?;
        if response.get("error").is_some_and(|error| !error.is_null()) {
            let code = response["error"]["code"].as_str().unwrap_or_default();
            return match code {
                "turn_pending" => Ok(WaitOutcome::Pending),
                "turn_cancelled" => Ok(WaitOutcome::Cancelled(response)),
                "approval_pending" => Ok(WaitOutcome::ApprovalPending),
                _ => Err(format!("JSONL request {id} failed: {}", response["error"])),
            };
        }
        Ok(WaitOutcome::Completed(response))
    }

    pub(crate) fn take_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn append_response_events(&mut self, response: &Value) {
        if let Some(events) = response
            .get("result")
            .and_then(|result| result.get("events"))
            .and_then(Value::as_array)
        {
            self.events.extend(events.iter().cloned());
        }
    }

    pub(crate) async fn finish(mut self) -> Result<(), String> {
        drop(self.stdin);
        let status = self
            .child
            .wait()
            .await
            .map_err(|error| format!("wait for mini-harness server: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("mini-harness serve exited with {status}"))
        }
    }

    pub(crate) async fn abort(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    async fn send(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "JSONL request id overflow".to_owned())?;
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "protocol_version": 2,
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|error| format!("encode JSONL request: {error}"))?;
        line.push(b'\n');
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "JSONL request exceeds the {MAX_MESSAGE_BYTES} byte protocol limit"
            ));
        }
        self.stdin
            .write_all(&line)
            .await
            .map_err(|error| format!("write JSONL request: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("flush JSONL request: {error}"))?;
        Ok(id)
    }

    async fn read_response(&mut self, id: u64) -> Result<Value, String> {
        let value = self.read_wire_response(id).await?;
        if value.get("error").is_some_and(|error| !error.is_null()) {
            return Err(format!("JSONL request {id} failed: {}", value["error"]));
        }
        Ok(value)
    }

    async fn read_wire_response(&mut self, id: u64) -> Result<Value, String> {
        loop {
            let value = read_line(&mut self.reader).await?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(value);
            }
            if value.get("error").is_some_and(|error| !error.is_null()) {
                return Err(format!("JSONL request {id} failed: {}", value["error"]));
            }
            if value.get("method").and_then(Value::as_str) == Some("event") {
                self.events.push(value);
            }
        }
    }
}

async fn read_line(reader: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .await
        .map_err(|error| format!("read JSONL response: {error}"))?;
    if bytes == 0 {
        return Err("mini-harness server exited before the response".into());
    }
    serde_json::from_str(&line).map_err(|error| format!("decode JSONL response: {error}"))
}
