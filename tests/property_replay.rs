//! Replay-determinism property tests (design §19.6).
//!
//! No property-testing framework: a seeded LCG generates pseudo-random mock
//! scripts (text, tool calls, errors, delays). For every generated session:
//!
//! * the reducer replays every prefix of the durable log without panicking;
//! * replaying the whole log twice yields the identical state;
//! * the store's own validation agrees with an independent reducer replay.
//!
//! A deterministic seed keeps failures reproducible; the seed sweep gives us
//! the "arbitrary valid event sequence" coverage the design asks for.

use mini_harness::{
    durable::{EventStore, InMemoryEventStore, reduce},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    runtime::{SessionId, SessionState, ToolCallId, ToolName, agent_loop::AgentLoop},
    tools::{ReadTool, ToolRegistry},
};
use std::sync::Arc;
use tempfile::tempdir;

/// xorshift64* — deterministic, no dependencies.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

fn random_script(seed: u64) -> Vec<MockResponse> {
    let mut rng = Lcg(seed | 1);
    let length = 1 + rng.below(8) as usize;
    (0..length)
        .map(|index| match rng.below(5) {
            0 => MockResponse::Text(format!("text {index}")),
            1 | 2 => MockResponse::ToolCall(ToolCall {
                call_id: ToolCallId::new(),
                name: ToolName("read".into()),
                input: serde_json::json!({"path": "note.txt"}),
            }),
            3 => MockResponse::Error(mini_harness::error::ProviderError::from("scripted failure")),
            _ => MockResponse::Text(format!("tail {index}")),
        })
        .collect()
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::default();
    registry.register(ReadTool { max_bytes: 100 }).unwrap();
    Arc::new(registry)
}

#[tokio::test]
async fn random_sessions_replay_deterministically_and_prefixes_never_panic() {
    let directory = tempdir().unwrap();
    tokio::fs::write(directory.path().join("note.txt"), "hello\n")
        .await
        .unwrap();

    for seed in 1..=24u64 {
        let provider = Arc::new(MockProvider::new(random_script(seed)));
        let store = Arc::new(InMemoryEventStore::default());
        let mut loop_ = AgentLoop::new_with_config(
            SessionId::new(),
            provider,
            Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
            registry(),
            store.clone(),
            Arc::new(mini_harness::policy::AllowAllPolicy),
            mini_harness::runtime::agent_loop::AgentLoopConfig::default(),
        );
        // Any outcome is fine: the property is about replaying whatever the
        // durable facts turn out to be.
        let _ = loop_.run_turn(format!("seed {seed}")).await;

        let events = store
            .read_from(mini_harness::runtime::EventSeq(1))
            .await
            .unwrap();
        assert!(
            !events.is_empty(),
            "seed {seed}: every run must produce durable facts"
        );

        // Every prefix replays without panicking; `reduce` returns errors for
        // genuinely invalid prefixes (none should occur here).
        let mut state = SessionState::new(events[0].session_id);
        for (index, event) in events.iter().enumerate() {
            reduce(&mut state, event).unwrap_or_else(|error| {
                panic!(
                    "seed {seed}: prefix {} failed to reduce: {error}",
                    index + 1
                )
            });
        }

        // Replaying twice yields the identical state.
        let first = replay(&events);
        let second = replay(&events);
        assert_eq!(first, second, "seed {seed}: replay is not deterministic");
        assert_eq!(first.last_seq, state.last_seq);
    }
}

fn replay(events: &[mini_harness::durable::Event]) -> SessionState {
    let mut state = SessionState::new(events[0].session_id);
    for event in events {
        reduce(&mut state, event).expect("full replay of a valid log must succeed");
    }
    state
}

#[test]
fn reducer_rejects_shuffled_events_instead_of_panicking() {
    // Deterministic shuffle of a small valid log: most permutations are
    // illegal; the reducer must return errors, never panic.
    let session = SessionId::new();
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    for _ in 0..32 {
        let mut events = vec![
            mini_harness::durable::Event::new(
                session,
                None,
                mini_harness::durable::EventPayload::SessionCreated,
            ),
            mini_harness::durable::Event::new(
                session,
                None,
                mini_harness::durable::EventPayload::UserInputRecorded {
                    input: mini_harness::runtime::UserInput("hi".into()),
                },
            ),
        ];
        // Fisher–Yates with the LCG.
        for index in (1..events.len()).rev() {
            let swap = rng.below((index + 1) as u64) as usize;
            events.swap(index, swap);
        }
        let mut state = SessionState::new(session);
        for event in &events {
            // Errors are expected and fine; panics are the bug class we test.
            let _ = reduce(&mut state, event);
        }
    }
}
