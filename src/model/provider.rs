use crate::{
    error::ProviderError,
    runtime::{
        ids::ToolCallId,
        state::{HistoryItem, ProviderContinuation},
        types::ToolName,
    },
    tools::ToolSpec,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub input: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelRequest {
    /// Optional per-request model override (design §6.1). `None` means the
    /// provider's own configuration decides; when set, providers must use it
    /// instead of their configured default.
    pub model: Option<crate::runtime::ModelName>,
    pub system_instructions: Option<String>,
    pub history: Vec<HistoryItem>,
    pub tools: Vec<ToolSpec>,
    pub continuation: Option<ProviderContinuation>,
}

impl ModelRequest {
    /// Builds a request with the current optional fields at their defaults.
    /// Prefer this constructor over struct literals so provider integrations
    /// remain source-compatible when another request-scoped option is added.
    pub fn new(history: Vec<HistoryItem>, tools: Vec<ToolSpec>) -> Self {
        Self {
            model: None,
            system_instructions: None,
            history,
            tools,
            continuation: None,
        }
    }

    pub fn with_continuation(mut self, continuation: Option<ProviderContinuation>) -> Self {
        self.continuation = continuation;
        self
    }

    /// Sets a per-request model override.
    pub fn with_model(mut self, model: Option<crate::runtime::ModelName>) -> Self {
        self.model = model;
        self
    }
}
#[derive(Clone, Debug)]
pub enum ModelResponse {
    Text(String),
    ToolCall(ToolCall),
    ToolCalls(Vec<ToolCall>),
    TextWithToolCalls { text: String, calls: Vec<ToolCall> },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderAttempt {
    pub attempt: u32,
    pub client_request_id: String,
    pub request_id: Option<String>,
    pub response_id: Option<String>,
    pub outcome: String,
    pub usage: Option<ModelUsage>,
}

#[derive(Clone, Debug)]
pub struct ModelCompletion {
    pub response: ModelResponse,
    pub continuation: Option<ProviderContinuation>,
    pub request_id: Option<String>,
    pub usage: Option<ModelUsage>,
    pub attempts: Vec<ProviderAttempt>,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Maximum time a single provider request should occupy an agent turn.
    fn request_timeout(&self) -> Duration {
        Duration::from_secs(120)
    }

    async fn complete(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ProviderError>;

    async fn complete_with_metadata(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelCompletion, ProviderError> {
        let continuation = request.continuation.clone();
        let response = self.complete(request, cancel).await?;
        Ok(ModelCompletion {
            response,
            continuation,
            request_id: None,
            usage: None,
            attempts: Vec::new(),
        })
    }

    async fn complete_observed(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        _observer: ProviderObserver,
    ) -> Result<ModelCompletion, ProviderError> {
        self.complete_with_metadata(request, cancel).await
    }
}

/// Request-scoped durable attempt reporting. Each acknowledgement is sent only
/// after the session actor has appended the event.
pub struct ProviderObserver {
    pub(crate) sender:
        tokio::sync::mpsc::Sender<(ProviderAttempt, tokio::sync::oneshot::Sender<()>)>,
}
impl ProviderObserver {
    pub async fn record(&self, attempt: ProviderAttempt) -> Result<(), ProviderError> {
        let (ack, done) = tokio::sync::oneshot::channel();
        self.sender
            .send((attempt, ack))
            .await
            .map_err(|_| ProviderError::from("provider observer closed"))?;
        done.await
            .map_err(|_| ProviderError::from("provider attempt was not persisted"))
    }
}
