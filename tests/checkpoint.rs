use std::sync::Arc;

use mini_harness::{
    durable::{EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse},
    policy::DefaultPolicy,
    runtime::{SessionStatus, UserInput},
    tools::ToolRegistry,
};
use tempfile::tempdir;

#[tokio::test]
async fn failed_turn_can_be_retried_in_the_same_session() {
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Error("temporary provider failure".into()),
        MockResponse::Text("recovered".into()),
    ]));
    let workspace = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = mini_harness::runtime::session::Session::new(
        provider,
        Arc::new(LocalExecutor::new(workspace.path().into())),
        Arc::new(ToolRegistry::default()),
        store.clone(),
    );

    assert!(session.start_turn("first attempt".into()).await.is_err());
    assert_eq!(session.state().status, SessionStatus::Failed);

    let (_, text) = session.start_turn("retry".into()).await.unwrap();
    assert_eq!(text, "recovered");
    assert_eq!(session.state().status, SessionStatus::Idle);
    assert_eq!(store.last_seq().await.unwrap(), session.state().last_seq);
}

#[tokio::test]
async fn session_resume_uses_checkpoint_state_and_replays_after_marker() {
    let workspace = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = mini_harness::runtime::session::Session::new(
        Arc::new(MockProvider::new(vec![MockResponse::Text("first".into())])),
        Arc::new(LocalExecutor::new(workspace.path().into())),
        Arc::new(ToolRegistry::default()),
        store.clone(),
    );
    session
        .start_turn("before checkpoint".into())
        .await
        .unwrap();

    let checkpoint_path = workspace.path().join("checkpoints/latest.json");
    let checkpoint = session.create_checkpoint(&checkpoint_path).await.unwrap();
    assert_eq!(checkpoint.state, *session.state());

    let resumed = mini_harness::runtime::session::Session::resume_with_policy_and_checkpoint(
        Arc::new(MockProvider::new(vec![])),
        Arc::new(LocalExecutor::new(workspace.path().into())),
        Arc::new(ToolRegistry::default()),
        store,
        checkpoint.session_id,
        &checkpoint_path,
        Arc::new(DefaultPolicy),
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state(), &checkpoint.state);
    assert_eq!(resumed.state().history.len(), 2);
    assert_eq!(
        resumed.state().history[0],
        mini_harness::runtime::HistoryItem::User(UserInput("before checkpoint".into()))
    );
}
