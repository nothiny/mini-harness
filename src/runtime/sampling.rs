use super::agent_loop::{AgentLoop, CancellationReason};
use crate::{
    durable::{EventPayload, EventStore},
    error::{HarnessError, ProviderError},
    executor::Executor,
    model::{ModelCompletion, ModelProvider, ModelRequest, ProviderObserver},
    runtime::TurnId,
};
use std::sync::Arc;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

impl<P: ModelProvider + 'static, E: Executor + 'static, S: EventStore + 'static>
    AgentLoop<P, E, S>
{
    pub(super) async fn sample_model(
        &mut self,
        request: ModelRequest,
        turn: TurnId,
        cancel: CancellationToken,
        turn_deadline: Instant,
        reason: CancellationReason,
    ) -> Result<ModelCompletion, HarnessError> {
        let (sender, mut receiver) = mpsc::channel(1);
        let provider = Arc::clone(&self.provider);
        let token = cancel.clone();
        let mut task = tokio::spawn(async move {
            provider
                .complete_observed(request, token, ProviderObserver { sender })
                .await
        });
        let deadline = (Instant::now() + self.provider.request_timeout()).min(turn_deadline);
        let mut channel_open = true;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    task.abort(); let _ = task.await;
                    self.append_cancellation(turn, reason).await?;
                    return Err(HarnessError::Cancelled);
                }
                _ = sleep_until(deadline) => {
                    task.abort(); let _ = task.await;
                    self.append(Some(turn), EventPayload::TurnTimedOut).await?;
                    return Err(HarnessError::Timeout);
                }
                record = receiver.recv(), if channel_open => {
                    let Some((attempt, ack)) = record else { channel_open = false; continue; };
                    let usage = attempt.usage.as_ref();
                    let result = self.append(Some(turn), EventPayload::ProviderAttemptRecorded {
                        attempt: attempt.attempt, client_request_id: attempt.client_request_id,
                        request_id: attempt.request_id, response_id: attempt.response_id, outcome: attempt.outcome,
                        input_tokens: usage.and_then(|u| u.input_tokens), output_tokens: usage.and_then(|u| u.output_tokens), total_tokens: usage.and_then(|u| u.total_tokens),
                    }).await;
                    if let Err(error) = result { task.abort(); let _ = task.await; return Err(error); }
                    let _ = ack.send(());
                }
                result = &mut task => {
                    let result = result.unwrap_or_else(|_| Err(ProviderError::from("provider task failed")));
                    match result {
                        Ok(completion) => return Ok(completion),
                        Err(error) => {
                            self.append(Some(turn), EventPayload::TurnFailed { error: error.to_string() }).await?;
                            return Err(HarnessError::Provider(error));
                        }
                    }
                }
            }
        }
    }
}
