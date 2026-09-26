use super::provider::{
    ModelCompletion, ModelRequest, ModelResponse, ProviderAttempt, ProviderObserver,
};
use crate::error::ProviderError;
use openai_protocol::{
    build_cc_request_body, build_request_body, is_local_endpoint, make_cc_continuation,
    make_continuation, parse_cc_json_response, parse_cc_stream_response, parse_json_response,
    parse_retry_after, parse_stream_response, retryable_status,
};
use reqwest::{Client, Response};
use serde_json::Value;
use std::{fmt, time::Duration};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[path = "openai_protocol.rs"]
mod openai_protocol;

const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const DEFAULT_DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";

/// Which OpenAI-compatible wire format this provider speaks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WireStyle {
    /// The Responses API (`input` array, `previous_response_id`).
    #[default]
    Responses,
    /// The chat-completions API (`messages`, `tool_calls`); used by DeepSeek
    /// and every other chat-completions-compatible server.
    ChatCompletions,
}

#[derive(Clone)]
pub struct OpenAiProviderConfig {
    pub model: String,
    api_key: String,
    pub endpoint: String,
    pub style: WireStyle,
    pub stream: bool,
    pub max_retries: u32,
    pub request_timeout: Duration,
    pub retry_base: Duration,
}

impl OpenAiProviderConfig {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            endpoint: DEFAULT_ENDPOINT.into(),
            style: WireStyle::Responses,
            stream: false,
            max_retries: 2,
            request_timeout: Duration::from_secs(120),
            retry_base: Duration::from_millis(250),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ProviderError::from("OPENAI_API_KEY is not set"))?;
        Ok(Self::new(model, api_key))
    }

    /// DeepSeek (chat-completions compatible) from `DEEPSEEK_API_KEY`.
    pub fn deepseek_from_env(model: impl Into<String>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| ProviderError::from("DEEPSEEK_API_KEY is not set"))?;
        Ok(Self::new(model, api_key)
            .with_endpoint(DEFAULT_DEEPSEEK_ENDPOINT)
            .with_wire_style(WireStyle::ChatCompletions))
    }

    pub fn with_wire_style(mut self, style: WireStyle) -> Self {
        self.style = style;
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_retry_base(mut self, retry_base: Duration) -> Self {
        self.retry_base = retry_base;
        self
    }
}

impl fmt::Debug for OpenAiProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProviderConfig")
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .field("stream", &self.stream)
            .field("max_retries", &self.max_retries)
            .field("request_timeout", &self.request_timeout)
            .field("retry_base", &self.retry_base)
            .finish()
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    client: Client,
    config: OpenAiProviderConfig,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .finish()
    }
}

impl OpenAiProvider {
    pub fn new(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::from_config(OpenAiProviderConfig::from_env(model)?)
    }

    /// A DeepSeek provider (`deepseek-chat` supports function calling;
    /// `deepseek-reasoner` reasoning output is dropped, tool calls are not
    /// supported by that model).
    pub fn deepseek(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::from_config(OpenAiProviderConfig::deepseek_from_env(model)?)
    }

    pub fn from_config(config: OpenAiProviderConfig) -> Result<Self, ProviderError> {
        if config.model.trim().is_empty() {
            return Err(ProviderError::from("OpenAI model name cannot be empty"));
        }
        if config.api_key.trim().is_empty() {
            return Err(ProviderError::from("OpenAI API key cannot be empty"));
        }
        if config.endpoint.trim().is_empty() {
            return Err(ProviderError::from("OpenAI endpoint cannot be empty"));
        }
        let url = reqwest::Url::parse(&config.endpoint)
            .map_err(|_| ProviderError::from("invalid OpenAI endpoint"))?;
        if (!is_local_endpoint(&config.endpoint) && url.scheme() != "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "OpenAI endpoint must use HTTPS and contain no credentials or query".into(),
            );
        }
        if config.max_retries > 5 || config.request_timeout.is_zero() {
            return Err("invalid OpenAI retry or timeout configuration".into());
        }
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .map_err(|_| ProviderError::from("invalid OpenAI API key"))?;
        let mut client_builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout);
        if is_local_endpoint(&config.endpoint) {
            client_builder = client_builder.no_proxy();
        }
        let client = client_builder.build().map_err(|error| {
            ProviderError::from(format!("failed to build OpenAI client: {error}"))
        })?;
        Ok(Self { client, config })
    }

    async fn execute_attempt(
        &self,
        body: &Value,
        client_id: &str,
    ) -> Result<(openai_protocol::ParsedResponse, Option<String>), AttemptFailure> {
        let response = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(&self.config.api_key)
            .header("x-client-request-id", client_id)
            .json(body)
            .send()
            .await
            .map_err(|error| AttemptFailure {
                message: "OpenAI transport failed",
                retry: error.is_connect() || error.is_timeout() || error.is_body(),
                retry_after: None,
                request_id: None,
            })?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .filter(|id| {
                id.len() <= 256
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            .map(str::to_owned);
        let body = read_body(response).await.map_err(|mut failure| {
            failure.request_id = request_id.clone();
            failure
        })?;
        if !status.is_success() {
            return Err(AttemptFailure {
                message: match status.as_u16() {
                    // Surface the server's error message for 400s —
                    // model-name and format issues are the common cause.
                    400 => {
                        let detail = serde_json::from_str::<Value>(&body)
                            .ok()
                            .and_then(|v| {
                                v.get("error")
                                    .and_then(|e| e.get("message"))
                                    .and_then(|m| m.as_str())
                                    .map(str::to_owned)
                            })
                            .unwrap_or_else(|| "bad request".into());
                        Box::leak(format!("HTTP 400: {detail}").into_boxed_str())
                    }
                    401 => "OpenAI HTTP 401",
                    403 => "OpenAI HTTP 403",
                    429 => "OpenAI HTTP 429",
                    _ => "OpenAI HTTP request failed",
                },
                retry: retryable_status(status, &body),
                retry_after,
                request_id,
            });
        }
        let parsed = match (self.config.style, self.config.stream) {
            (WireStyle::Responses, false) => parse_json_response(&body),
            (WireStyle::Responses, true) => parse_stream_response(&body),
            (WireStyle::ChatCompletions, false) => parse_cc_json_response(&body),
            (WireStyle::ChatCompletions, true) => parse_cc_stream_response(&body),
        }
        .map_err(|_| AttemptFailure {
            message: "OpenAI returned an invalid or incomplete response",
            retry: false,
            retry_after: None,
            request_id: request_id.clone(),
        })?;
        Ok((parsed, request_id))
    }

    async fn complete_request(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        observer: Option<ProviderObserver>,
    ) -> Result<ModelCompletion, ProviderError> {
        let body = match self.config.style {
            WireStyle::Responses => build_request_body(&self.config, &request)?,
            WireStyle::ChatCompletions => build_cc_request_body(&self.config, &request)?,
        };
        let mut attempts = Vec::new();
        for number in 1..=self.config.max_retries + 1 {
            if cancel.is_cancelled() {
                return Err("OpenAI request cancelled".into());
            }
            let mut attempt = ProviderAttempt {
                attempt: number,
                client_request_id: format!("mini-harness-{}", Uuid::new_v4()),
                request_id: None,
                response_id: None,
                outcome: "started".into(),
                usage: None,
            };
            if let Some(observer) = &observer {
                observer.record(attempt.clone()).await?;
            }
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("OpenAI request cancelled".into()),
                result = self.execute_attempt(&body, &attempt.client_request_id) => result,
            };
            match result {
                Ok((parsed, request_id)) => {
                    let continuation = match self.config.style {
                        WireStyle::Responses => make_continuation(
                            &request,
                            &self.config,
                            &parsed.calls,
                            &parsed.response_id,
                        ),
                        // Chat completions always replays history; the
                        // continuation only keeps the cursor uniform.
                        WireStyle::ChatCompletions => make_cc_continuation(
                            &request,
                            &self.config,
                            &parsed.calls,
                            parsed.reasoning_content.as_deref(),
                        ),
                    };
                    attempt.request_id = request_id.clone();
                    attempt.response_id = Some(parsed.response_id);
                    attempt.usage = parsed.usage.clone();
                    attempt.outcome = "completed".into();
                    if let Some(observer) = &observer {
                        observer.record(attempt.clone()).await?;
                    }
                    attempts.push(attempt);
                    let calls = parsed
                        .calls
                        .into_iter()
                        .map(|call| call.call)
                        .collect::<Vec<_>>();
                    let response = if calls.is_empty() {
                        ModelResponse::Text(parsed.text)
                    } else if parsed.text.is_empty() && calls.len() == 1 {
                        ModelResponse::ToolCall(calls.into_iter().next().unwrap())
                    } else if parsed.text.is_empty() {
                        ModelResponse::ToolCalls(calls)
                    } else {
                        ModelResponse::TextWithToolCalls {
                            text: parsed.text,
                            calls,
                        }
                    };
                    return Ok(ModelCompletion {
                        response,
                        continuation: Some(continuation),
                        request_id,
                        usage: parsed.usage,
                        attempts,
                    });
                }
                Err(failure) => {
                    let retry = failure.retry && number <= self.config.max_retries;
                    tracing::warn!(
                        target: "mini_harness::provider",
                        provider = "openai",
                        attempt = number,
                        retry,
                        message = failure.message,
                        request_id = ?failure.request_id,
                        "openai attempt failed"
                    );
                    attempt.request_id = failure.request_id;
                    attempt.outcome = if retry { "retry" } else { "failed" }.into();
                    if let Some(observer) = &observer {
                        observer.record(attempt.clone()).await?;
                    }
                    attempts.push(attempt);
                    if !retry {
                        return Err(failure.message.into());
                    }
                    let delay = failure.retry_after.unwrap_or_else(|| {
                        self.config
                            .retry_base
                            .saturating_mul(2_u32.pow(number - 1))
                            .min(Duration::from_secs(30))
                    });
                    tokio::select! { _ = cancel.cancelled() => return Err("OpenAI request cancelled".into()), _ = sleep(delay) => {} }
                }
            }
        }
        Err("OpenAI retry budget exhausted".into())
    }
}

struct AttemptFailure {
    message: &'static str,
    retry: bool,
    retry_after: Option<Duration>,
    request_id: Option<String>,
}

async fn read_body(mut response: Response) -> Result<String, AttemptFailure> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| AttemptFailure {
        message: "OpenAI response connection interrupted",
        retry: true,
        retry_after: None,
        request_id: None,
    })? {
        if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(AttemptFailure {
                message: "OpenAI response exceeds byte limit",
                retry: false,
                retry_after: None,
                request_id: None,
            });
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| AttemptFailure {
        message: "OpenAI response is not UTF-8",
        retry: false,
        retry_after: None,
        request_id: None,
    })
}

#[async_trait::async_trait]
impl super::provider::ModelProvider for OpenAiProvider {
    fn request_timeout(&self) -> Duration {
        self.config.request_timeout
    }

    async fn complete(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(self.complete_request(request, cancel, None).await?.response)
    }
    async fn complete_with_metadata(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelCompletion, ProviderError> {
        self.complete_request(request, cancel, None).await
    }
    async fn complete_observed(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        observer: ProviderObserver,
    ) -> Result<ModelCompletion, ProviderError> {
        self.complete_request(request, cancel, Some(observer)).await
    }
}
