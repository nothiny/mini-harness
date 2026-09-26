use super::{
    MockProvider, MockResponse, ModelCompletion, ModelProvider, ModelRequest, ModelResponse,
    OpenAiProvider, ProviderObserver,
};
use crate::error::ProviderError;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Provider selected by a command-line or protocol configuration.
///
/// Keeping the selection behind one concrete provider type lets the runtime
/// retain its generic, statically dispatched API while allowing a session to
/// choose between the deterministic mock and OpenAI at startup.
#[derive(Clone)]
pub enum ConfiguredProvider {
    Mock(MockProvider),
    OpenAi(OpenAiProvider),
    /// DeepSeek over the chat-completions wire format.
    Deepseek(OpenAiProvider),
}

impl ConfiguredProvider {
    pub fn mock() -> Self {
        Self::Mock(MockProvider::repeating(MockResponse::Text(
            "mock response".into(),
        )))
    }

    pub fn openai(model: impl Into<String>) -> Result<Self, ProviderError> {
        Ok(Self::OpenAi(OpenAiProvider::new(model)?))
    }

    pub fn deepseek(model: impl Into<String>) -> Result<Self, ProviderError> {
        Ok(Self::Deepseek(OpenAiProvider::deepseek(model)?))
    }
}

#[async_trait::async_trait]
impl ModelProvider for ConfiguredProvider {
    fn request_timeout(&self) -> Duration {
        match self {
            Self::Mock(provider) => provider.request_timeout(),
            Self::OpenAi(provider) => provider.request_timeout(),
            Self::Deepseek(provider) => provider.request_timeout(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ProviderError> {
        match self {
            Self::Mock(provider) => provider.complete(request, cancel).await,
            Self::OpenAi(provider) => provider.complete(request, cancel).await,
            Self::Deepseek(provider) => provider.complete(request, cancel).await,
        }
    }

    async fn complete_with_metadata(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelCompletion, ProviderError> {
        match self {
            Self::Mock(provider) => provider.complete_with_metadata(request, cancel).await,
            Self::OpenAi(provider) => provider.complete_with_metadata(request, cancel).await,
            Self::Deepseek(provider) => provider.complete_with_metadata(request, cancel).await,
        }
    }

    async fn complete_observed(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        observer: ProviderObserver,
    ) -> Result<ModelCompletion, ProviderError> {
        match self {
            Self::Mock(provider) => provider.complete_observed(request, cancel, observer).await,
            Self::OpenAi(provider) => provider.complete_observed(request, cancel, observer).await,
            Self::Deepseek(provider) => provider.complete_observed(request, cancel, observer).await,
        }
    }
}

impl std::fmt::Debug for ConfiguredProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mock(_) => formatter.write_str("ConfiguredProvider::Mock"),
            Self::OpenAi(provider) => formatter
                .debug_tuple("ConfiguredProvider::OpenAi")
                .field(provider)
                .finish(),
            Self::Deepseek(provider) => formatter
                .debug_tuple("ConfiguredProvider::Deepseek")
                .field(provider)
                .finish(),
        }
    }
}
