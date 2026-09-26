use crate::{
    model::ModelRequest,
    runtime::{
        ModelText, ToolResult, UserInput,
        state::{HistoryItem, ProviderContinuation, SessionState},
    },
    tools::ToolSpec,
};

/// Conservative byte cap for one model-visible context item.
///
/// The runtime does not depend on a provider tokenizer, so the byte limit is
/// deliberately below the documented 10K-token ceiling. Durable history keeps
/// the original value; only the provider-facing snapshot is bounded.
pub const MAX_CONTEXT_ITEM_BYTES: usize = 16 * 1024;
/// Only the most recent bounded history is copied into a provider request.
/// Continuation requests retain the suffix beginning at their provider cursor.
pub const MAX_CONTEXT_HISTORY_BYTES: usize = 512 * 1024;
const TRUNCATION_MARKER: &str = "\n[context item truncated]";

/// An owned, immutable input for one model request.
///
/// The runtime creates a fresh snapshot for every provider call. Providers
/// receive the snapshot's owned request and must not read session state or the
/// event store to fill in context.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextSnapshot {
    system_instructions: Option<String>,
    history: Vec<HistoryItem>,
    tools: Vec<ToolSpec>,
    continuation: Option<ProviderContinuation>,
}

impl ContextSnapshot {
    pub fn from_state(state: &SessionState, tools: Vec<ToolSpec>) -> Self {
        let start = history_start(state);
        let history = state
            .history
            .iter()
            .skip(start)
            .cloned()
            .map(bound_history_item)
            .collect();
        let continuation = state.provider_continuation.clone().map(|mut continuation| {
            continuation.history_cursor = continuation.history_cursor.saturating_sub(start);
            continuation
        });
        Self {
            system_instructions: None,
            history,
            tools,
            continuation,
        }
    }

    pub fn from_state_with_system_instructions(
        state: &SessionState,
        tools: Vec<ToolSpec>,
        system_instructions: impl Into<String>,
    ) -> Self {
        Self::from_state(state, tools).with_system_instructions(system_instructions)
    }

    pub fn with_system_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.system_instructions = Some(bound_text(&instructions.into()));
        self
    }

    pub fn system_instructions(&self) -> Option<&str> {
        self.system_instructions.as_deref()
    }

    pub fn history(&self) -> &[HistoryItem] {
        &self.history
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn into_model_request(self) -> ModelRequest {
        ModelRequest {
            model: None,
            system_instructions: self.system_instructions,
            history: self.history,
            tools: self.tools,
            continuation: self.continuation,
        }
    }
}

fn bound_history_item(item: HistoryItem) -> HistoryItem {
    match item {
        HistoryItem::User(input) => HistoryItem::User(UserInput(bound_text(&input.0))),
        HistoryItem::Assistant(text) => HistoryItem::Assistant(ModelText(bound_text(&text.0))),
        HistoryItem::Tool {
            call_id,
            name,
            result,
        } => HistoryItem::Tool {
            call_id,
            name,
            result: ToolResult(bound_text(&result.0)),
        },
    }
}

fn history_start(state: &SessionState) -> usize {
    let continuation_start = state
        .provider_continuation
        .as_ref()
        .map(|continuation| continuation.history_cursor.min(state.history.len()));
    let mut size = 2usize;
    let mut start = state.history.len();
    for (index, item) in state.history.iter().enumerate().rev() {
        let item_size =
            serde_json::to_vec(item).map_or(MAX_CONTEXT_ITEM_BYTES, |bytes| bytes.len());
        let next_size = size
            .saturating_add(item_size)
            .saturating_add(if start != state.history.len() { 1 } else { 0 });
        if next_size > MAX_CONTEXT_HISTORY_BYTES && index + 1 < state.history.len() {
            break;
        }
        start = index;
        size = next_size;
        if continuation_start.is_some_and(|cursor| start <= cursor) {
            break;
        }
    }
    continuation_start.map_or(start, |cursor| start.min(cursor))
}

fn bound_text(value: &str) -> String {
    if value.len() <= MAX_CONTEXT_ITEM_BYTES {
        return value.to_owned();
    }
    let marker_start = MAX_CONTEXT_ITEM_BYTES.saturating_sub(TRUNCATION_MARKER.len());
    let mut end = marker_start;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], TRUNCATION_MARKER)
}
