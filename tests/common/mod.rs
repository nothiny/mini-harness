//! Shared test helpers: a tiny one-request-per-fixture HTTP server for
//! driving the OpenAI provider offline.

use serde_json::Value;
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

pub(crate) struct Fixture {
    pub(crate) status: u16,
    pub(crate) body: String,
    pub(crate) stall: bool,
}

pub(crate) struct FixtureServer {
    pub(crate) endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl FixtureServer {
    pub(crate) fn ok(body: String) -> Fixture {
        Fixture {
            status: 200,
            body,
            stall: false,
        }
    }

    pub(crate) fn status(code: u16, body: &str) -> Fixture {
        Fixture {
            status: code,
            body: body.to_owned(),
            stall: false,
        }
    }

    /// Accepts the request and never answers (drives cancellation/timeouts).
    pub(crate) fn stall() -> Fixture {
        Fixture {
            status: 0,
            body: String::new(),
            stall: true,
        }
    }

    pub(crate) async fn spawn(fixtures: Vec<Fixture>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        tokio::spawn(async move {
            for fixture in fixtures {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = read_request_body(&mut stream).await;
                let payload = body
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|index| &body[index + 4..])
                    .unwrap_or(&body[..]);
                if let Ok(value) = serde_json::from_slice::<Value>(payload) {
                    captured.lock().await.push(value);
                }
                if fixture.stall {
                    // Hold the connection open until the client goes away.
                    let mut buffer = [0u8; 64];
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        stream.read(&mut buffer),
                    )
                    .await;
                    continue;
                }
                let status_text = match fixture.status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    _ => "Test Response",
                };
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    fixture.status,
                    status_text,
                    fixture.body.len(),
                    fixture.body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        Self { endpoint, requests }
    }

    pub(crate) async fn requests(&self) -> Vec<Value> {
        self.requests.lock().await.clone()
    }
}

async fn read_request_body(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until the end of the request: headers, then the JSON body. The
    // mock keeps it simple by reading until the connection would block on a
    // complete request (clients send one request per connection here).
    loop {
        let read = match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            stream.read(&mut chunk),
        )
        .await
        {
            Ok(Ok(read)) => read,
            _ => break,
        };
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            let header_end = buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|index| index + 4)
                .unwrap_or(0);
            if let Some(length) = content_length(&buffer[..header_end]) {
                if buffer.len() >= header_end + length {
                    break;
                }
            }
        }
    }
    buffer
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    let length = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    })?;
    Some(length)
}
