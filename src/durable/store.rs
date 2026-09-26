use super::event::Event;
use crate::error::DurableError;
use crate::runtime::types::EventSeq;
use async_trait::async_trait;
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, event: Event) -> Result<Event, DurableError>;
    async fn read_from(&self, seq: EventSeq) -> Result<Vec<Event>, DurableError>;
    async fn last_seq(&self) -> Result<EventSeq, DurableError>;
}
