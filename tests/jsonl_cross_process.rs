//! Cross-process edge cases for the JSONL event store's incremental
//! validation: another process appending, crashing mid-line, repairing, and
//! appending again — while a warm cache exists in the first process.

use mini_harness::{
    durable::{Event, EventPayload, EventStore, JsonlEventStore},
    runtime::{EventSeq, SessionId, TurnId, UserInput},
};
use tempfile::tempdir;

/// 可重复追加的事件: turn 内的 provider 尝试记录（reducer 允许多次）。
fn repeatable_event(session: SessionId, turn: TurnId, text: &str) -> Event {
    Event::new(
        session,
        Some(turn),
        EventPayload::ProviderAttemptRecorded {
            attempt: 1,
            client_request_id: text.into(),
            request_id: None,
            response_id: None,
            outcome: "ok".into(),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        },
    )
}

/// 创建日志前缀: session.created + user.input + turn.started，
/// 之后可以合法地追加任意多个 provider 尝试事件。
async fn seeded_prefix(path: &std::path::Path, session: SessionId) -> (JsonlEventStore, TurnId) {
    let store = JsonlEventStore::new(path.to_path_buf());
    store
        .append(Event::new(session, None, EventPayload::SessionCreated))
        .await
        .unwrap();
    store
        .append(Event::new(
            session,
            None,
            EventPayload::UserInputRecorded {
                input: UserInput("warm".into()),
            },
        ))
        .await
        .unwrap();
    let turn = TurnId::new();
    store
        .append(Event::new(session, Some(turn), EventPayload::TurnStarted))
        .await
        .unwrap();
    (store, turn)
}

/// Process B appends, crashes mid-line, and repairs: process A's warm cache
/// must notice the shrink (repair) and fall back to a full validation
/// instead of trusting its stale validated offset.
#[tokio::test]
async fn repair_by_another_process_forces_a_full_revalidation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shared.jsonl");
    let session = SessionId::new();
    let (a, turn) = seeded_prefix(&path, session).await;
    for index in 0..5 {
        a.append(repeatable_event(session, turn, &format!("a{index}")))
            .await
            .unwrap();
    }
    let replays_before = a.full_replays();

    // Process B appends a valid event; A validates it via the suffix path.
    let b = JsonlEventStore::new(path.clone());
    b.append(repeatable_event(session, turn, "b0"))
        .await
        .unwrap();
    let events = a.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 9);
    assert_eq!(a.suffix_validations(), 1);
    assert_eq!(a.full_replays(), replays_before);

    // Process B crashes mid-append, leaving a partial last line…
    let garbage = br#"{"schema_version":3,"event_id":"evt_half"#;
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(garbage).unwrap();
    }
    // …and repairs it (dropping only the partial tail).
    assert!(b.repair_partial_tail().await.unwrap());
    assert_eq!(b.read_from(EventSeq(1)).await.unwrap().len(), 9);

    // A's validated offset now equals the repaired length: the suffix path
    // no longer applies, so the next read must do a full revalidation.
    let replays_mid = a.full_replays();
    let events = a.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 9, "repair must not lose complete events");
    assert!(
        a.full_replays() > replays_mid,
        "a shrunk file must force full revalidation, not a stale cache hit"
    );

    // Appending still works afterwards with continuous sequence numbers.
    a.append(repeatable_event(session, turn, "a-after-repair"))
        .await
        .unwrap();
    let events = a.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 10);
    assert_eq!(events.last().unwrap().seq, EventSeq(10));
}

/// Repeated appends by another process are each handled by one suffix
/// validation; the full-replay counter stays flat the whole time.
#[tokio::test]
async fn repeated_cross_process_appends_stay_on_the_suffix_path() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shared.jsonl");
    let session = SessionId::new();
    let (a, turn) = seeded_prefix(&path, session).await;
    let replays_before = a.full_replays();

    let b = JsonlEventStore::new(path.clone());
    for round in 0..3 {
        b.append(repeatable_event(session, turn, &format!("round{round}")))
            .await
            .unwrap();
        let events = a.read_from(EventSeq(1)).await.unwrap();
        assert_eq!(events.len(), 4 + round);
        assert_eq!(a.suffix_validations(), round as u64 + 1);
    }
    assert_eq!(a.full_replays(), replays_before);
}

/// A mid-line crash that is *not* repaired must surface as corruption for
/// every reader — including one with a warm, fully validated prefix.
#[tokio::test]
async fn unrepaired_partial_tail_is_corruption_even_with_warm_cache() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shared.jsonl");
    let session = SessionId::new();
    let (a, turn) = seeded_prefix(&path, session).await;
    a.append(repeatable_event(session, turn, "one"))
        .await
        .unwrap();
    assert!(a.read_from(EventSeq(1)).await.is_ok());

    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"schema_version":3,"trunc"#).unwrap();
    }

    // The partial tail is corruption for a fresh reader…
    assert!(
        JsonlEventStore::new(path.clone())
            .read_from(EventSeq(1))
            .await
            .is_err()
    );
    // …and for the warm reader whose cache ends before the damage.
    assert!(a.read_from(EventSeq(1)).await.is_err());
}
