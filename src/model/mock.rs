use super::provider::*;
use crate::error::ProviderError;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug)]
pub enum MockResponse {
    Text(String),
    ToolCall(ToolCall),
    ToolCalls(Vec<ToolCall>),
    Error(ProviderError),
    Delay(Duration),
}
#[derive(Clone, Default)]
pub struct MockProvider {
    script: Arc<Mutex<Vec<MockResponse>>>,
    repeat: Arc<Mutex<Option<MockResponse>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
impl MockProvider {
    pub fn new(script: Vec<MockResponse>) -> Self {
        Self {
            script: Arc::new(Mutex::new(script)),
            repeat: Default::default(),
            requests: Default::default(),
        }
    }

    /// Creates a deterministic provider that returns the same response for
    /// every completion after the client-configured script is exhausted.
    pub fn repeating(response: MockResponse) -> Self {
        Self {
            script: Default::default(),
            repeat: Arc::new(Mutex::new(Some(response))),
            requests: Default::default(),
        }
    }
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}
#[async_trait]
impl ModelProvider for MockProvider {
    async fn complete(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        loop {
            let item = {
                let mut script = self.script.lock().unwrap();
                match script.first().cloned() {
                    Some(item) => {
                        script.remove(0);
                        item
                    }
                    None => self
                        .repeat
                        .lock()
                        .unwrap()
                        .clone()
                        .ok_or_else(|| ProviderError::from("mock response script exhausted"))?,
                }
            };
            match item {
                MockResponse::Text(text) => return Ok(ModelResponse::Text(text)),
                MockResponse::ToolCall(call) => return Ok(ModelResponse::ToolCall(call)),
                MockResponse::ToolCalls(calls) => return Ok(ModelResponse::ToolCalls(calls)),
                MockResponse::Error(error) => return Err(error),
                MockResponse::Delay(duration) => {
                    tokio::select! { _ = sleep(duration) => {}, _ = cancel.cancelled() => return Err(ProviderError::from("cancelled")) }
                }
            }
        }
    }

    /// Synthesizes a mock continuation so contract tests can exercise the
    /// tool-result round trip against every provider uniformly (§19.2).
    async fn complete_with_metadata(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelCompletion, ProviderError> {
        let history_len = request.history.len();
        let response = self.complete(request, cancel).await?;
        let continuation = match &response {
            ModelResponse::ToolCall(call) => {
                Some(mock_continuation(std::iter::once(call), history_len))
            }
            ModelResponse::ToolCalls(calls) | ModelResponse::TextWithToolCalls { calls, .. } => {
                Some(mock_continuation(calls.iter(), history_len))
            }
            ModelResponse::Text(_) => None,
        };
        Ok(ModelCompletion {
            response,
            continuation,
            request_id: None,
            usage: None,
            attempts: Vec::new(),
        })
    }
}

fn mock_continuation<'a>(
    calls: impl Iterator<Item = &'a ToolCall>,
    history_cursor: usize,
) -> crate::runtime::ProviderContinuation {
    crate::runtime::ProviderContinuation {
        provider: "mock".into(),
        response_id: Some("mock_resp".into()),
        model: "mock-model".into(),
        endpoint: "mock".into(),
        native_call_ids: calls
            .map(|call| (call.call_id, format!("native_{}", call.call_id)))
            .collect(),
        history_cursor,
        reasoning_content: None,
    }
}
