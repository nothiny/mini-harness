//! Cross-process edge cases for the JSONL event store's incremental
//! validation: another process appending, crashing mid-line, repairing, and
//! appending again — while a warm cache exists in the first process.

use mini_harness::{
    durable::{Event, EventPayload, EventStore, JsonlEventStore},
    runtime::{EventSeq, SessionId, UserInput},
};
use tempfile::tempdir;

fn input_event(session: SessionId, text: &str) -> Event {
    Event::new(
        session,
        None,
        EventPayload::UserInputRecorded {
            input: UserInput(text.into()),
        },
    )
}

/// Process B appends, crashes mid-line, and repairs: process A's warm cache
/// must notice the shrink (repair) and fall back to a full validation
/// instead of trusting its stale validated offset.
#[tokio::test]
async fn repair_by_another_process_forces_a_full_revalidation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shared.jsonl");
    let session = SessionId::new();

    let a = JsonlEventStore::new(path.clone());
    a.append(Event::new(session, None, EventPayload::SessionCreated))
        .await
        .unwrap();
    for index in 0..5 {
        a.append(input_event(session, &format!("a{index}")))
            .await
            .unwrap();
    }
    let replays_before = a.full_replays();

    // Process B appends a valid event; A validates it via the suffix path.
    let b = JsonlEventStore::new(path.clone());
    b.append(input_event(session, "b0")).await.unwrap();
    let events = a.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 7);
    assert_eq!(a.suffix_validations(), 1);
    assert_eq!(a.full_replays(), replays_before);

    // Process B crashes mid-append, leaving a partial last line…
    let garbage = br#"{"schema_version":2,"event_id":"evt_half"#;
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
    assert_eq!(b.read_from(EventSeq(1)).await.unwrap().len(), 7);

    // A's validated offset now equals the repaired length: the suffix path
    // no longer applies, so the next read must do a full revalidation.
    let replays_mid = a.full_replays();
    let events = a.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 7, "repair must not lose complete events");
    assert!(
        a.full_replays() > replays_mid,
        "a shrunk file must force full revalidation, not a stale cache hit"
    );

    // Appending still works afterwards with continuous sequence numbers.
    a.append(input_event(session, "a-after-repair"))
        .await
        .unwrap();
    let events = a.read_from(EventSeq(1)).await.unwrap();
    assert_eq!(events.len(), 8);
    assert_eq!(events.last().unwrap().seq, EventSeq(8));
}

/// Repeated appends by another process are each handled by one suffix
/// validation; the full-replay counter stays flat the whole time.
#[tokio::test]
async fn repeated_cross_process_appends_stay_on_the_suffix_path() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shared.jsonl");
    let session = SessionId::new();

    let a = JsonlEventStore::new(path.clone());
    a.append(Event::new(session, None, EventPayload::SessionCreated))
        .await
        .unwrap();
    let replays_before = a.full_replays();

    let b = JsonlEventStore::new(path.clone());
    for round in 0..3 {
        b.append(input_event(session, &format!("round{round}")))
            .await
            .unwrap();
        let events = a.read_from(EventSeq(1)).await.unwrap();
        assert_eq!(events.len(), 2 + round);
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

    let a = JsonlEventStore::new(path.clone());
    a.append(Event::new(session, None, EventPayload::SessionCreated))
        .await
        .unwrap();
    a.append(input_event(session, "one")).await.unwrap();
    assert!(a.read_from(EventSeq(1)).await.is_ok());

    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"schema_version":2,"trunc"#).unwrap();
    }

    let result = a.read_from(EventSeq(1)).await;
    assert!(
        result.is_err(),
        "a partial tail must be rejected, never silently served from cache"
    );
    assert!(a.last_seq().await.is_err());
}
