//! Shared provider behavior tests (design §19.2).
//!
//! Every provider — the deterministic mock and the OpenAI Responses client
//! (against a local mock HTTP server) — passes the same contract: text
//! responses, single and multiple tool calls, provider errors, cancellation,
//! and tool-result continuation.

mod common;

use common::FixtureServer;
use mini_harness::model::{
    MockProvider, MockResponse, ModelProvider, ModelRequest, ModelResponse, OpenAiProvider,
    OpenAiProviderConfig, ToolCall,
};
use mini_harness::runtime::{HistoryItem, ModelText, ToolName, ToolResult, UserInput};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn history() -> Vec<HistoryItem> {
    vec![HistoryItem::User(UserInput("hello contract".into()))]
}

fn read_call() -> ToolCall {
    ToolCall {
        call_id: mini_harness::runtime::ToolCallId::new(),
        name: ToolName("read".into()),
        input: json!({"path": "notes.txt"}),
    }
}

fn request(history: Vec<HistoryItem>) -> ModelRequest {
    ModelRequest::new(history, vec![])
}

fn openai_text_body(text: &str, response_id: &str) -> String {
    json!({
        "id": response_id,
        "status": "completed",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": text}],
        }],
    })
    .to_string()
}

fn openai_calls_body(calls: &[Value], response_id: &str) -> String {
    json!({
        "id": response_id,
        "status": "completed",
        "output": calls,
    })
    .to_string()
}

async fn mock_provider() -> Arc<dyn ModelProvider> {
    Arc::new(MockProvider::repeating(MockResponse::Text(
        "hello contract".into(),
    )))
}

// ---------------------------------------------------------------------------
// The contract, parameterized by provider.
// ---------------------------------------------------------------------------

async fn contract_text(provider: &dyn ModelProvider) {
    let response = provider
        .complete(request(history()), CancellationToken::new())
        .await
        .unwrap();
    let ModelResponse::Text(text) = response else {
        panic!("expected a text response, got {response:?}");
    };
    assert!(!text.trim().is_empty());
}

async fn contract_single_tool_call(provider: &dyn ModelProvider) {
    let response = provider
        .complete(request(history()), CancellationToken::new())
        .await
        .unwrap();
    let ModelResponse::ToolCall(call) = response else {
        panic!("expected one tool call, got {response:?}");
    };
    assert_eq!(call.name.0, "read");
    assert_eq!(call.input["path"], "notes.txt");
}

async fn contract_multiple_tool_calls(provider: &dyn ModelProvider) {
    let response = provider
        .complete(request(history()), CancellationToken::new())
        .await
        .unwrap();
    let calls = match response {
        ModelResponse::ToolCalls(calls) => calls,
        ModelResponse::ToolCall(call) => vec![call],
        other => panic!("expected tool calls, got {other:?}"),
    };
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.name.0 == "read"));
}

async fn contract_provider_error(provider: &dyn ModelProvider) {
    let result = provider
        .complete(request(history()), CancellationToken::new())
        .await;
    assert!(result.is_err(), "expected a provider error");
}

async fn contract_cancellation(provider: Arc<dyn ModelProvider>) {
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let mut task = tokio::spawn(async move {
        provider
            .complete(request(history()), token)
            .await
            .map(|_| ())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
    match outcome {
        Ok(Ok(result)) => assert!(result.is_err(), "cancelled request must error"),
        _ => panic!("cancelled request did not finish"),
    }
    task.abort();
}

async fn contract_tool_result_continuation(provider: &dyn ModelProvider) {
    let first = provider
        .complete_with_metadata(request(history()), CancellationToken::new())
        .await
        .unwrap();
    let call = match &first.response {
        ModelResponse::ToolCall(call) => call.clone(),
        other => panic!("expected a tool call, got {other:?}"),
    };
    let continuation = first
        .continuation
        .clone()
        .expect("tool-call response must produce a continuation");
    let mut follow_up_history = history();
    follow_up_history.push(HistoryItem::Assistant(ModelText(format!(
        "tool call: {}",
        call.name.0
    ))));
    follow_up_history.push(HistoryItem::Tool {
        call_id: call.call_id,
        name: call.name.clone(),
        result: ToolResult("{\"text\":\"file content\"}".into()),
    });
    let follow_up = request(follow_up_history).with_continuation(Some(continuation));
    let response = provider
        .complete(follow_up, CancellationToken::new())
        .await
        .unwrap();
    let ModelResponse::Text(_) = response else {
        panic!("expected final text after tool result, got {response:?}");
    };
}

// ---------------------------------------------------------------------------
// Mock provider runs the contract.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_provider_passes_the_contract() {
    let text = mock_provider().await;
    contract_text(text.as_ref()).await;

    let single = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(read_call())]));
    contract_single_tool_call(single.as_ref()).await;

    let multiple = Arc::new(MockProvider::new(vec![MockResponse::ToolCalls(vec![
        read_call(),
        read_call(),
    ])]));
    contract_multiple_tool_calls(multiple.as_ref()).await;

    let failing = Arc::new(MockProvider::new(vec![MockResponse::Error(
        mini_harness::error::ProviderError::from("mock failure"),
    )]));
    contract_provider_error(failing.as_ref()).await;

    let delayed = Arc::new(MockProvider::new(vec![MockResponse::Delay(
        Duration::from_secs(30),
    )]));
    contract_cancellation(Arc::clone(&delayed) as Arc<dyn ModelProvider>).await;

    let continuation = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(read_call()),
        MockResponse::Text("done".into()),
    ]));
    contract_tool_result_continuation(continuation.as_ref()).await;
}

// ---------------------------------------------------------------------------
// OpenAI provider (local mock HTTP server) runs the same contract.
// ---------------------------------------------------------------------------

async fn openai_provider(server: &FixtureServer) -> OpenAiProvider {
    OpenAiProvider::from_config(
        OpenAiProviderConfig::new("contract-model", "sk-contract")
            .with_endpoint(server.endpoint.clone())
            .with_request_timeout(Duration::from_secs(5))
            .with_max_retries(0),
    )
    .unwrap()
}

#[tokio::test]
async fn openai_provider_passes_the_contract() {
    // 1. text
    let server = FixtureServer::spawn(vec![FixtureServer::ok(openai_text_body(
        "hello contract",
        "resp_text",
    ))])
    .await;
    let provider = openai_provider(&server).await;
    contract_text(&provider).await;

    // 2. single tool call
    let server = FixtureServer::spawn(vec![FixtureServer::ok(openai_calls_body(
        &[json!({
            "type": "function_call",
            "call_id": "call_a",
            "name": "read",
            "arguments": "{\"path\":\"notes.txt\"}",
        })],
        "resp_call",
    ))])
    .await;
    let provider = openai_provider(&server).await;
    contract_single_tool_call(&provider).await;

    // 3. multiple tool calls
    let server = FixtureServer::spawn(vec![FixtureServer::ok(openai_calls_body(
        &[
            json!({
                "type": "function_call",
                "call_id": "call_b",
                "name": "read",
                "arguments": "{\"path\":\"a.txt\"}",
            }),
            json!({
                "type": "function_call",
                "call_id": "call_c",
                "name": "read",
                "arguments": "{\"path\":\"b.txt\"}",
            }),
        ],
        "resp_calls",
    ))])
    .await;
    let provider = openai_provider(&server).await;
    contract_multiple_tool_calls(&provider).await;

    // 4. provider error (400 is never retried)
    let server = FixtureServer::spawn(vec![FixtureServer::status(400, "{\"error\":{}}")]).await;
    let provider = openai_provider(&server).await;
    contract_provider_error(&provider).await;

    // 5. cancellation (server stalls; client request timeout bounds the wait)
    let server = FixtureServer::spawn(vec![FixtureServer::stall()]).await;
    let provider = openai_provider(&server).await;
    contract_cancellation(Arc::new(provider)).await;

    // 6. tool-result continuation carries previous_response_id
    let server = FixtureServer::spawn(vec![
        FixtureServer::ok(openai_calls_body(
            &[json!({
                "type": "function_call",
                "call_id": "call_d",
                "name": "read",
                "arguments": "{\"path\":\"notes.txt\"}",
            })],
            "resp_cont",
        )),
        FixtureServer::ok(openai_text_body("done", "resp_final")),
    ])
    .await;
    let provider = openai_provider(&server).await;
    contract_tool_result_continuation(&provider).await;
    let requests = server.requests().await;
    assert!(
        requests.len() >= 2,
        "continuation must issue a follow-up request"
    );
    let follow_up = requests.last().unwrap();
    assert_eq!(
        follow_up["previous_response_id"], "resp_cont",
        "follow-up must reference the previous response"
    );
    let outputs = follow_up["input"].as_array().unwrap();
    assert!(
        outputs
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["output"]
                    .as_str()
                    .unwrap_or("")
                    .contains("file content")),
        "follow-up must carry the tool result: {outputs:?}"
    );
}

// Keep the Value import used in fixture assertions.
#[allow(dead_code)]
fn _unused(value: Value) -> Value {
    value
}

// ---------------------------------------------------------------------------
// DeepSeek (chat completions) runs the same contract.
// ---------------------------------------------------------------------------

fn cc_text_body(text: &str, id: &str) -> String {
    json!({
        "id": id,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
    })
    .to_string()
}

fn cc_calls_body(calls: &[Value], id: &str) -> String {
    json!({
        "id": id,
        "choices": [{"index": 0, "message": {"role": "assistant", "tool_calls": calls}, "finish_reason": "tool_calls"}],
    })
    .to_string()
}

async fn cc_provider(server: &FixtureServer) -> OpenAiProvider {
    OpenAiProvider::from_config(
        OpenAiProviderConfig::new("deepseek-chat", "sk-contract")
            .with_endpoint(server.endpoint.clone())
            .with_wire_style(mini_harness::model::WireStyle::ChatCompletions)
            .with_request_timeout(Duration::from_secs(5))
            .with_max_retries(0),
    )
    .unwrap()
}

#[tokio::test]
async fn deepseek_chat_completions_passes_the_contract() {
    let text = FixtureServer::spawn(vec![FixtureServer::ok(cc_text_body(
        "hello contract",
        "cc_text",
    ))])
    .await;
    contract_text(&cc_provider(&text).await).await;

    let single = FixtureServer::spawn(vec![FixtureServer::ok(cc_calls_body(
        &[json!({
            "id": "cc_call_a",
            "type": "function",
            "function": {"name": "read", "arguments": "{\"path\":\"notes.txt\"}"},
        })],
        "cc_call",
    ))])
    .await;
    contract_single_tool_call(&cc_provider(&single).await).await;

    let multiple = FixtureServer::spawn(vec![FixtureServer::ok(cc_calls_body(
        &[
            json!({"id": "cc_call_b", "type": "function",
                  "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"}}),
            json!({"id": "cc_call_c", "type": "function",
                  "function": {"name": "read", "arguments": "{\"path\":\"b.txt\"}"}}),
        ],
        "cc_calls",
    ))])
    .await;
    contract_multiple_tool_calls(&cc_provider(&multiple).await).await;

    let failing = FixtureServer::spawn(vec![FixtureServer::status(400, "{\"error\":{}}")]).await;
    contract_provider_error(&cc_provider(&failing).await).await;

    let stalled = FixtureServer::spawn(vec![FixtureServer::stall()]).await;
    contract_cancellation(Arc::new(cc_provider(&stalled).await)).await;

    let continuation = FixtureServer::spawn(vec![
        FixtureServer::ok(cc_calls_body(
            &[json!({
                "id": "cc_call_d",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"notes.txt\"}"},
            })],
            "cc_cont",
        )),
        FixtureServer::ok(cc_text_body("done", "cc_final")),
    ])
    .await;
    let provider = cc_provider(&continuation).await;
    contract_tool_result_continuation(&provider).await;
    let requests = continuation.requests().await;
    assert!(requests.len() >= 2);
    let follow_up = requests.last().unwrap();
    assert!(
        follow_up.get("previous_response_id").is_none(),
        "chat completions is stateless and must not reference previous responses"
    );
    let messages = follow_up["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| message["role"] == "tool"
            && message["content"]
                .as_str()
                .unwrap_or("")
                .contains("file content")),
        "follow-up must carry the tool result: {messages:?}"
    );
}
