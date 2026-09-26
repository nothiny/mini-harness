use super::{event::Event, store::EventStore};
use crate::{error::DurableError, runtime::types::EventSeq};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
#[derive(Clone, Default)]
pub struct InMemoryEventStore {
    events: Arc<Mutex<Vec<Event>>>,
}
#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(&self, mut event: Event) -> Result<Event, DurableError> {
        let mut e = self.events.lock().unwrap();
        event.seq = EventSeq(e.len() as u64 + 1);
        e.push(event.clone());
        Ok(event)
    }
    async fn read_from(&self, seq: EventSeq) -> Result<Vec<Event>, DurableError> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.seq.0 >= seq.0)
            .cloned()
            .collect())
    }
    async fn last_seq(&self) -> Result<EventSeq, DurableError> {
        Ok(EventSeq(self.events.lock().unwrap().len() as u64))
    }
}
