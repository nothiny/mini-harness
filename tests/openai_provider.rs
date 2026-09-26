use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{
        ModelProvider, ModelRequest, ModelResponse, OpenAiProvider, OpenAiProviderConfig, WireStyle,
    },
    runtime::session::Session,
    runtime::{
        HistoryItem, ModelText, ProviderContinuation, ToolCallId, ToolName, ToolResult, UserInput,
    },
    tools::{ReadTool, RiskClass, ToolRegistry, ToolSpec},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

struct ResponseFixture {
    status: u16,
    content_type: &'static str,
    headers: &'static str,
    body: String,
}

#[test]
fn openai_endpoint_validation_documents_safe_migration_boundary() {
    assert!(
        OpenAiProvider::from_config(
            OpenAiProviderConfig::new("test-model", "sk-test")
                .with_endpoint("http://127.0.0.1:8080/v1/responses")
        )
        .is_ok()
    );
    for endpoint in [
        "http://api.example.test/v1/responses",
        "https://api.openai.com/v1/responses?tenant=test",
        "https://user:pass@api.openai.com/v1/responses",
    ] {
        let result = OpenAiProvider::from_config(
            OpenAiProviderConfig::new("test-model", "sk-test").with_endpoint(endpoint),
        );
        assert!(result.is_err(), "endpoint should be rejected: {endpoint}");
    }
    assert!(
        OpenAiProvider::from_config(
            OpenAiProviderConfig::new("test-model", "sk-test").with_max_retries(6)
        )
        .is_err()
    );
}

async fn spawn_server(
    fixtures: Vec<ResponseFixture>,
) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        for fixture in fixtures {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = read_request_body(&mut stream).await;
            captured
                .lock()
                .await
                .push(serde_json::from_slice(&body).expect("provider request should be JSON"));
            let status_text = match fixture.status {
                200 => "OK",
                400 => "Bad Request",
                401 => "Unauthorized",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                fixture.status,
                status_text,
                fixture.content_type,
                fixture.body.len(),
                fixture.headers,
                fixture.body,
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (endpoint, requests, task)
}

async fn read_request_body(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .or_else(|| line.strip_prefix("Content-Length:"))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap();
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    bytes[header_end..header_end + content_length].to_vec()
}

fn request(history: Vec<HistoryItem>, continuation: Option<ProviderContinuation>) -> ModelRequest {
    ModelRequest {
        model: None,
        system_instructions: Some("be concise".into()),
        history,
        tools: vec![ToolSpec {
            name: ToolName("read".into()),
            description: "read a file".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            risk: RiskClass::Read,
        }],
        continuation,
    }
}

#[tokio::test]
async fn openai_parses_text_usage_and_redacts_key() {
    let response = json!({
        "id": "resp_text",
        "status": "completed",
        "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
    });
    let (endpoint, requests, server) = spawn_server(vec![ResponseFixture {
        status: 200,
        content_type: "application/json",
        headers: "x-request-id: req_text\r\n",
        body: response.to_string(),
    }])
    .await;
    let config = OpenAiProviderConfig::new("test-model", "sk-test-secret")
        .with_endpoint(endpoint)
        .with_max_retries(0);
    let provider = OpenAiProvider::from_config(config).unwrap();
    let completion = provider
        .complete_with_metadata(
            request(vec![HistoryItem::User(UserInput("hello".into()))], None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.await.unwrap();

    assert!(matches!(completion.response, ModelResponse::Text(ref text) if text == "hello"));
    assert_eq!(completion.request_id.as_deref(), Some("req_text"));
    assert_eq!(
        completion
            .usage
            .as_ref()
            .and_then(|usage| usage.total_tokens),
        Some(5)
    );
    assert_eq!(completion.attempts.len(), 1);
    assert_eq!(requests.lock().await[0]["model"], "test-model");
    assert_eq!(requests.lock().await[0]["instructions"], "be concise");
    assert!(!format!("{provider:?}").contains("sk-test-secret"));
}

#[tokio::test]
async fn openai_continues_tool_output_with_native_call_id() {
    let first = json!({
        "id": "resp_tool",
        "status": "completed",
        "output": [{"type":"function_call","id":"fc_1","call_id":"call_native_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}]
    });
    let second = json!({
        "id": "resp_final",
        "status": "completed",
        "output": [{"type":"message","content":[{"type":"output_text","text":"done"}]}]
    });
    let (endpoint, requests, server) = spawn_server(vec![
        ResponseFixture {
            status: 200,
            content_type: "application/json",
            headers: "",
            body: first.to_string(),
        },
        ResponseFixture {
            status: 200,
            content_type: "application/json",
            headers: "",
            body: second.to_string(),
        },
    ])
    .await;
    let provider = OpenAiProvider::from_config(
        OpenAiProviderConfig::new("test-model", "sk-test")
            .with_endpoint(endpoint)
            .with_max_retries(0),
    )
    .unwrap();
    let first_completion = provider
        .complete_with_metadata(
            request(vec![HistoryItem::User(UserInput("read it".into()))], None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let call = match first_completion.response.clone() {
        ModelResponse::ToolCall(call) => call,
        response => panic!("unexpected response: {response:?}"),
    };
    let second_request = request(
        vec![
            HistoryItem::User(UserInput("read it".into())),
            HistoryItem::Assistant(ModelText("tool call: read".into())),
            HistoryItem::Tool {
                call_id: call.call_id,
                name: call.name.clone(),
                result: ToolResult("file contents".into()),
            },
        ],
        first_completion.continuation,
    );
    let second_completion = provider
        .complete_with_metadata(second_request, CancellationToken::new())
        .await
        .unwrap();
    server.await.unwrap();
    assert!(matches!(second_completion.response, ModelResponse::Text(text) if text == "done"));
    let captured = requests.lock().await;
    assert_eq!(captured[1]["previous_response_id"], "resp_tool");
    assert_eq!(captured[1]["input"][0]["type"], "function_call_output");
    assert_eq!(captured[1]["input"][0]["call_id"], "call_native_1");
    assert_eq!(captured[1]["input"][0]["output"], "file contents");
    assert!(
        !captured[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "assistant")
    );
}

#[tokio::test]
async fn openai_replays_history_when_continuation_configuration_changes() {
    let response = json!({
        "id": "resp_replayed",
        "status": "completed",
        "output": [{"type":"message","content":[{"type":"output_text","text":"replayed"}]}]
    });
    let (endpoint, requests, server) = spawn_server(vec![ResponseFixture {
        status: 200,
        content_type: "application/json",
        headers: "",
        body: response.to_string(),
    }])
    .await;
    let provider = OpenAiProvider::from_config(
        OpenAiProviderConfig::new("new-model", "sk-test")
            .with_endpoint(endpoint.clone())
            .with_max_retries(0),
    )
    .unwrap();
    let request = request(
        vec![
            HistoryItem::User(UserInput("read it".into())),
            HistoryItem::Assistant(ModelText("tool call: read".into())),
            HistoryItem::Tool {
                call_id: ToolCallId::new(),
                name: ToolName("read".into()),
                result: ToolResult("file contents".into()),
            },
        ],
        Some(ProviderContinuation {
            provider: "openai".into(),
            response_id: Some("resp_from_old_endpoint".into()),
            model: "old-model".into(),
            endpoint: "https://api.openai.com/v1/responses".into(),
            native_call_ids: Default::default(),
            history_cursor: 1,
            reasoning_content: None,
        }),
    );
    let completion = provider
        .complete_with_metadata(request, CancellationToken::new())
        .await
        .unwrap();
    server.await.unwrap();

    assert!(matches!(completion.response, ModelResponse::Text(text) if text == "replayed"));
    let captured = requests.lock().await;
    assert!(captured[0].get("previous_response_id").is_none());
    let input = captured[0]["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[2]["role"], "user");
    assert_eq!(input[2]["content"], "Tool `read` result:\nfile contents");
}

#[tokio::test]
async fn openai_parses_streaming_text_and_retries_transient_429() {
    let stream = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream\",\"status\":\"in_progress\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"response_id\":\"resp_stream\",\"delta\":\"hel\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"response_id\":\"resp_stream\",\"delta\":\"lo\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, _, server) = spawn_server(vec![ResponseFixture {
        status: 200,
        content_type: "text/event-stream",
        headers: "",
        body: stream.into(),
    }])
    .await;
    let provider = OpenAiProvider::from_config(
        OpenAiProviderConfig::new("test-model", "sk-test")
            .with_endpoint(endpoint)
            .with_streaming(true)
            .with_max_retries(0),
    )
    .unwrap();
    let completion = provider
        .complete_with_metadata(
            request(vec![HistoryItem::User(UserInput("hello".into()))], None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert!(matches!(completion.response, ModelResponse::Text(text) if text == "hello"));
    assert_eq!(completion.usage.unwrap().total_tokens, Some(3));

    let (endpoint, _, server) = spawn_server(vec![
        ResponseFixture { status: 429, content_type: "application/json", headers: "Retry-After: 0\r\n", body: r#"{"error":{"code":"rate_limit_exceeded"}}"#.into() },
        ResponseFixture { status: 200, content_type: "application/json", headers: "", body: json!({"id":"resp_retry","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}).to_string() },
    ]).await;
    let provider = OpenAiProvider::from_config(
        OpenAiProviderConfig::new("test-model", "sk-test")
            .with_endpoint(endpoint)
            .with_max_retries(1)
            .with_retry_base(Duration::ZERO),
    )
    .unwrap();
    let completion = provider
        .complete_with_metadata(
            request(vec![HistoryItem::User(UserInput("retry".into()))], None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert!(matches!(completion.response, ModelResponse::Text(text) if text == "ok"));
    assert_eq!(completion.attempts.len(), 2);
    assert_eq!(completion.attempts[0].outcome, "retry");
}

#[tokio::test]
async fn openai_provider_drives_runtime_and_persists_continuation() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("a.txt"), "hello from runtime")
        .await
        .unwrap();
    let first = json!({
        "id": "resp_runtime_tool",
        "status": "completed",
        "output": [{"type":"function_call","id":"fc_runtime","call_id":"call_runtime","name":"read","arguments":"{\"path\":\"a.txt\"}"}]
    });
    let second = json!({
        "id": "resp_runtime_final",
        "status": "completed",
        "output": [{"type":"message","content":[{"type":"output_text","text":"finished"}]}]
    });
    let (endpoint, requests, server) = spawn_server(vec![
        ResponseFixture {
            status: 200,
            content_type: "application/json",
            headers: "x-request-id: req_runtime_1\r\n",
            body: first.to_string(),
        },
        ResponseFixture {
            status: 200,
            content_type: "application/json",
            headers: "x-request-id: req_runtime_2\r\n",
            body: second.to_string(),
        },
    ])
    .await;
    let provider = Arc::new(
        OpenAiProvider::from_config(
            OpenAiProviderConfig::new("test-model", "sk-test")
                .with_endpoint(endpoint)
                .with_max_retries(0),
        )
        .unwrap(),
    );
    let mut registry = ToolRegistry::default();
    registry.register(ReadTool { max_bytes: 10_000 }).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        Arc::clone(&store),
    );

    let (_, text) = session.start_turn("read a.txt".into()).await.unwrap();
    server.await.unwrap();
    assert_eq!(text, "finished");
    assert_eq!(
        session
            .state()
            .provider_continuation
            .as_ref()
            .and_then(|continuation| continuation.response_id.as_deref()),
        Some("resp_runtime_final")
    );
    let events = store
        .read_from(mini_harness::runtime::EventSeq(1))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ProviderAttemptRecorded { .. }))
    );
    let captured = requests.lock().await;
    assert_eq!(captured[1]["previous_response_id"], "resp_runtime_tool");
    assert_eq!(captured[1]["input"][0]["call_id"], "call_runtime");
}

#[tokio::test]
async fn openai_does_not_retry_auth_errors() {
    let (endpoint, _, server) = spawn_server(vec![ResponseFixture {
        status: 401,
        content_type: "application/json",
        headers: "x-request-id: req_auth\r\n",
        body: r#"{"error":{"message":"secret body","code":"invalid_api_key"}}"#.into(),
    }])
    .await;
    let provider = OpenAiProvider::from_config(
        OpenAiProviderConfig::new("test-model", "sk-secret")
            .with_endpoint(endpoint)
            .with_max_retries(3)
            .with_retry_base(Duration::ZERO),
    )
    .unwrap();
    let error = provider
        .complete_with_metadata(
            request(vec![HistoryItem::User(UserInput("auth".into()))], None),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(!error.to_string().contains("secret body"));
    assert!(error.to_string().contains("401"));
}

// ---------------------------------------------------------------------------
// DeepSeek (chat-completions wire style)
// ---------------------------------------------------------------------------

fn cc_body(id: &str, content: Option<&str>, tool_calls: &[Value]) -> Value {
    let mut message = json!({"role": "assistant", "content": content});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    json!({
        "id": id,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    })
}

#[tokio::test]
async fn deepseek_parses_text_tool_calls_and_usage() {
    let body = cc_body(
        "cc_text_calls",
        Some("here is the plan"),
        &[json!({
            "id": "call_cc_1",
            "type": "function",
            "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"},
        })],
    );
    let (endpoint, _requests, server) = spawn_server(vec![ResponseFixture {
        status: 200,
        content_type: "application/json",
        headers: "x-request-id: req_cc_1\r\n",
        body: body.to_string(),
    }])
    .await;
    let provider = OpenAiProvider::from_config(
        OpenAiProviderConfig::new("deepseek-chat", "sk-test")
            .with_endpoint(endpoint)
            .with_wire_style(WireStyle::ChatCompletions)
            .with_max_retries(0),
    )
    .unwrap();
    let completion = provider
        .complete_with_metadata(
            request(vec![], None),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    server.await.unwrap();
    match completion.response {
        ModelResponse::TextWithToolCalls { text, calls } => {
            assert_eq!(text, "here is the plan");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name.0, "read");
            assert_eq!(calls[0].input["path"], "a.txt");
        }
        other => panic!("expected text with tool calls, got {other:?}"),
    }
    let usage = completion.usage.unwrap();
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.total_tokens, Some(18));
}

#[tokio::test]
async fn deepseek_round_trips_tool_results_as_chat_messages() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("a.txt"), "hello from deepseek")
        .await
        .unwrap();
    let first = cc_body(
        "cc_tool",
        None,
        &[json!({
            "id": "call_cc_tool",
            "type": "function",
            "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"},
        })],
    );
    let second = cc_body("cc_final", Some("all done"), &[]);
    let (endpoint, requests, server) = spawn_server(vec![
        ResponseFixture {
            status: 200,
            content_type: "application/json",
            headers: "",
            body: first.to_string(),
        },
        ResponseFixture {
            status: 200,
            content_type: "application/json",
            headers: "",
            body: second.to_string(),
        },
    ])
    .await;
    let config = OpenAiProviderConfig::new("deepseek-chat", "sk-test")
        .with_endpoint(endpoint)
        .with_wire_style(WireStyle::ChatCompletions)
        .with_max_retries(0);
    let provider = Arc::new(OpenAiProvider::from_config(config).unwrap());
    let mut registry = ToolRegistry::default();
    registry.register(ReadTool { max_bytes: 10_000 }).unwrap();
    let mut session = Session::new(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(registry),
        Arc::new(InMemoryEventStore::default()),
    );

    let (_, text) = session.start_turn("read a.txt".into()).await.unwrap();
    server.await.unwrap();
    assert_eq!(text, "all done");

    let requests = requests.lock().await.clone();
    assert_eq!(requests.len(), 2);
    let follow_up = &requests[1];
    // Chat completions has no server-side continuation.
    assert!(follow_up.get("previous_response_id").is_none());
    let messages = follow_up["messages"].as_array().unwrap();
    let has_tool_roundtrip = messages.windows(2).any(|pair| {
        let assistant = &pair[0];
        let tool = &pair[1];
        assistant["role"] == "assistant"
            && assistant["tool_calls"][0]["id"] == "call_cc_tool"
            && tool["role"] == "tool"
            && tool["tool_call_id"] == "call_cc_tool"
            && tool["content"]
                .as_str()
                .unwrap_or("")
                .contains("hello from deepseek")
    });
    assert!(
        has_tool_roundtrip,
        "follow-up must carry the assistant tool_calls + role:tool pair: {messages:?}"
    );
}

#[tokio::test]
async fn deepseek_streams_text_and_tool_call_deltas() {
    let sse = [
        r#"data: {"id":"cc_stream","choices":[{"index":0,"delta":{"role":"assistant","content":"部分"},"finish_reason":null}]}"#,
        r#"data: {"id":"cc_stream","choices":[{"index":0,"delta":{"content":"回答"},"finish_reason":null}]}"#,
        r#"data: {"id":"cc_stream","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_s1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"cc_stream","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":null}]}"#,
        "data: [DONE]",
    ]
    .join("\n\n");
    let (endpoint, _requests, server) = spawn_server(vec![ResponseFixture {
        status: 200,
        content_type: "text/event-stream",
        headers: "",
        body: sse,
    }])
    .await;
    let config = OpenAiProviderConfig::new("deepseek-chat", "sk-test")
        .with_endpoint(endpoint)
        .with_wire_style(WireStyle::ChatCompletions)
        .with_streaming(true)
        .with_max_retries(0);
    let provider = OpenAiProvider::from_config(config).unwrap();
    let response = provider
        .complete(
            request(vec![], None),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    server.await.unwrap();
    match response {
        ModelResponse::TextWithToolCalls { text, calls } => {
            assert_eq!(text, "部分回答");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name.0, "read");
            assert_eq!(calls[0].input["path"], "a.txt");
        }
        other => panic!("expected streamed text with tool calls, got {other:?}"),
    }
}
