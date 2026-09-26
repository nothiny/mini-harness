//! The cross-process cancel handshake, pinned end to end.
//!
//! `cancel` may run in another process against the same event log. The
//! contract (documented in implementation-plan §12.4):
//!
//! * the canceller appends `turn.cancel_requested` + `turn.cancelled`;
//! * the runner's next append fails the out-of-sync check and the turn ends
//!   with a fatal durable error **without writing anything on top**;
//! * the resulting log is exactly what the canceller wrote, and recovery
//!   classifies the session as clean.

use mini_harness::{
    durable::{EventPayload, EventStore, JsonlEventStore, inspect_store},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse},
    policy::DefaultPolicy,
    runtime::{EventSeq, session::Session},
    tools::ToolRegistry,
};
use std::{sync::Arc, time::Duration};
use tempfile::tempdir;

#[tokio::test]
async fn cross_process_cancel_stops_the_runner_without_overwriting_the_log() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let store_a = Arc::new(JsonlEventStore::new(path.clone()));

    // Runner: a slow provider keeps the turn active long enough to cancel.
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Delay(Duration::from_millis(400)),
        MockResponse::Text("never reached cleanly".into()),
    ]));
    let runner = Session::new_with_policy(
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        Arc::clone(&store_a),
        Arc::new(DefaultPolicy),
    );
    let session_id = runner.state().session_id;

    let run = {
        let mut runner = runner;
        tokio::spawn(async move { runner.start_turn("hello".into()).await })
    };

    // Wait until the runner has durably started its turn.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let turn_id = loop {
        let events = store_a.read_from(EventSeq(1)).await.unwrap();
        if let Some(turn) = events.iter().find_map(|event| {
            (matches!(event.payload, EventPayload::TurnStarted))
                .then(|| event.turn_id)
                .flatten()
        }) {
            break turn;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "runner never recorded turn.started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // A second process (its own store handle + restored session) cancels.
    let canceller_store = Arc::new(JsonlEventStore::new(path.clone()));
    let mut canceller = Session::resume(
        Arc::new(MockProvider::new(vec![MockResponse::Text("unused".into())])),
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        Arc::new(ToolRegistry::default()),
        Arc::clone(&canceller_store),
        session_id,
    )
    .await
    .unwrap();
    canceller.cancel_turn(turn_id).await.unwrap();

    // The runner wakes up, tries to append its response, and must fail
    // loudly instead of writing over the canceller's events.
    let outcome = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("runner must terminate after the out-of-sync failure")
        .unwrap();
    assert!(
        matches!(
            outcome,
            Err(ref error) if matches!(
                error,
                mini_harness::HarnessError::InvariantViolation(_)
                    | mini_harness::HarnessError::Durable(_)
            )
        ),
        "runner should fail with a durable/invariant error, got {outcome:?}"
    );

    // The log is exactly: create, input, start, cancel-requested, cancelled.
    let events = canceller_store.read_from(EventSeq(1)).await.unwrap();
    let kinds = events
        .iter()
        .map(|event| event.payload.kind())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "session.created",
            "user.input.recorded",
            "turn.started",
            "turn.cancel_requested",
            "turn.cancelled",
        ],
        "the runner must not append anything after the cross-process cancel"
    );

    // And recovery sees a terminated, clean session.
    let report = inspect_store(canceller_store.as_ref(), session_id)
        .await
        .unwrap();
    assert!(report.is_clean(), "findings: {:?}", report.findings);
    assert!(report.state.active_turn.is_none());
}
