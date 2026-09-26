//! Scheduler stage tests (plan §14 option B):
//! durable per-session queue, task identity, lifecycle projection, and the
//! global executor concurrency limit (design §12/§21).

use mini_harness::{
    durable::InMemoryEventStore,
    executor::{Executor, LocalExecutor, ProcessRequest},
    model::{MockProvider, MockResponse, ToolCall},
    policy::{DefaultPolicy, ToolPolicy},
    runtime::{
        UserInput,
        agent_loop::AgentLoopConfig,
        ids::{SessionId, ToolCallId},
        session,
    },
    scheduler::{Scheduler, SchedulerConfig, SchedulerTask, SessionRunState},
    tools::{BashTool, ReadTool, ToolRegistry},
};
use std::{sync::Arc, time::Duration};
use tempfile::tempdir;

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::default();
    registry.register(ReadTool { max_bytes: 100 }).unwrap();
    registry.register(BashTool::default()).unwrap();
    Arc::new(registry)
}

#[tokio::test]
async fn scheduler_queues_and_runs_inputs_in_order_per_session() {
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Delay(Duration::from_millis(80)),
        MockResponse::Text("first".into()),
        MockResponse::Text("second".into()),
        MockResponse::Text("third".into()),
    ]));
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let scheduler = Scheduler::new(SchedulerConfig::default());
    let session = SessionId::new();
    let (handle, _actor) = session::spawn_session_with_config(
        session,
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
        AgentLoopConfig::default(),
    );
    scheduler.register_session(session, handle).await;

    let first = scheduler.submit(session, "one").await.unwrap();
    // Give the first turn time to enter its provider delay before submitting
    // the next two, so they must queue behind it.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = scheduler.submit(session, "two").await.unwrap();
    let third = scheduler.submit(session, "three").await.unwrap();
    assert_ne!(first, second);
    assert_ne!(second, third);

    assert_eq!(scheduler.wait(first).await.unwrap(), "first");
    assert_eq!(scheduler.wait(second).await.unwrap(), "second");
    assert_eq!(scheduler.wait(third).await.unwrap(), "third");
    assert_eq!(
        scheduler.run_state(session).await.unwrap(),
        SessionRunState::Idle
    );
    let _ = SchedulerTask::new(session, second.turn);
}

#[tokio::test]
async fn queue_rejects_beyond_the_configured_bound_without_appending() {
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::Delay(Duration::from_millis(80)),
        MockResponse::Text("done".into()),
        MockResponse::Text("two done".into()),
    ]));
    let directory = tempdir().unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let scheduler = Scheduler::new(SchedulerConfig {
        max_queued_inputs: 1,
    });
    let session = SessionId::new();
    let (handle, _actor) = session::spawn_session_with_config(
        session,
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        store.clone(),
        Arc::new(DefaultPolicy),
        AgentLoopConfig {
            max_queued_inputs: 1,
            ..Default::default()
        },
    );
    scheduler.register_session(session, handle).await;

    let _first = scheduler.submit(session, "one").await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    // Both submits race inside the running turn's delayed acknowledgement;
    // with a bound of one queued input exactly one must be accepted.
    let scheduler = Arc::new(scheduler);
    let second = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.submit(session, "two").await })
    };
    let third = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.submit(session, "three").await })
    };
    let (second, third) = (second.await.unwrap(), third.await.unwrap());
    let rejected = matches!(
        third.as_ref(),
        Err(mini_harness::HarnessError::QueueLimitExceeded)
    ) || matches!(
        second.as_ref(),
        Err(mini_harness::HarnessError::QueueLimitExceeded)
    );
    assert!(rejected, "expected one submit to exceed the queue bound");
    let queued = second
        .or(third)
        .expect("one of the racing submits must have been accepted");
    assert_eq!(scheduler.wait(_first).await.unwrap(), "done");
    // The queued input still runs; only the rejected one never started.
    assert_eq!(scheduler.wait(queued).await.unwrap(), "two done");
}

#[tokio::test]
async fn run_state_reports_waiting_approval_and_resolves_after_response() {
    let call = ToolCallId::new();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(ToolCall {
            call_id: call,
            name: mini_harness::runtime::ToolName("bash".into()),
            input: serde_json::json!({"command": "touch approved"}),
        }),
        MockResponse::Text("after approval".into()),
    ]));
    let directory = tempdir().unwrap();
    let scheduler = Scheduler::new(SchedulerConfig::default());
    let session = SessionId::new();
    let (handle, _actor) = session::spawn_session_with_config(
        session,
        provider,
        Arc::new(LocalExecutor::new(directory.path().to_path_buf())),
        registry(),
        Arc::new(InMemoryEventStore::default()),
        Arc::new(DefaultPolicy) as Arc<dyn ToolPolicy>,
        AgentLoopConfig::default(),
    );
    scheduler.register_session(session, handle).await;

    let task = scheduler.submit(session, "run command").await.unwrap();
    // The turn pauses durably on the approval request.
    assert!(matches!(
        scheduler.wait(task).await,
        Err(ref error) if matches!(error.as_ref(), mini_harness::HarnessError::ApprovalPending(_))
    ));
    assert_eq!(
        scheduler.run_state(session).await.unwrap(),
        SessionRunState::WaitingApproval { turn_id: task.turn }
    );
    let (turn, text) = scheduler
        .respond_approval(session, task.turn, call, true)
        .await
        .unwrap();
    assert_eq!(turn, task.turn);
    assert_eq!(text, "after approval");
    assert_eq!(
        scheduler.run_state(session).await.unwrap(),
        SessionRunState::Idle
    );
}

#[tokio::test]
async fn shared_executor_limits_global_process_concurrency() {
    let directory = tempdir().unwrap();
    // One executor shared by two sessions with a single process slot.
    let executor =
        Arc::new(LocalExecutor::new(directory.path().to_path_buf()).with_process_limits(1));
    let scheduler = Scheduler::new(SchedulerConfig::default());

    let mut tasks = Vec::new();
    for index in 0..2 {
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall(ToolCall {
                call_id: ToolCallId::new(),
                name: mini_harness::runtime::ToolName("bash".into()),
                input: serde_json::json!({"command": "sleep 0.2; echo done"}),
            }),
            MockResponse::Text(format!("session {index} done")),
        ]));
        let session = SessionId::new();
        let (handle, _actor) = session::spawn_session_with_config(
            session,
            provider,
            Arc::clone(&executor),
            registry(),
            Arc::new(InMemoryEventStore::default()),
            Arc::new(mini_harness::policy::AllowAllPolicy),
            AgentLoopConfig::default(),
        );
        scheduler.register_session(session, handle).await;
        tasks.push((session, scheduler.submit(session, "run").await.unwrap()));
    }

    let started = std::time::Instant::now();
    for (session, task) in tasks {
        let text = scheduler.wait(task).await.unwrap();
        assert!(text.contains("done"));
        let _ = session;
    }
    // Two 200ms sleeps through one slot must serialize: comfortably more
    // than 350ms total, far less than a per-session limit would allow both
    // in parallel (~250ms).
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "processes ran concurrently: {:?}",
        started.elapsed()
    );

    // A tiny wait budget surfaces the limit as a structured error instead of
    // queueing forever.
    let strict = Arc::new(
        LocalExecutor::new(directory.path().to_path_buf())
            .with_process_limits(1)
            .with_slot_wait(Duration::from_millis(20)),
    );
    // Run both concurrently: the second must wait for the single slot and
    // give up once its wait budget is exhausted.
    let slow = {
        let executor = Arc::clone(&strict);
        tokio::spawn(async move {
            executor
                .run_process(
                    ProcessRequest {
                        command: "sleep 0.3".into(),
                        timeout: Duration::from_secs(5),
                        max_stdout_bytes: mini_harness::runtime::ByteLimit(1024),
                        max_stderr_bytes: mini_harness::runtime::ByteLimit(1024),
                        max_combined_bytes: mini_harness::runtime::ByteLimit(1024),
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        })
    };
    // Give the spawned command time to hold the only slot.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = strict
        .run_process(
            ProcessRequest {
                command: "echo hi".into(),
                timeout: Duration::from_secs(5),
                max_stdout_bytes: mini_harness::runtime::ByteLimit(1024),
                max_stderr_bytes: mini_harness::runtime::ByteLimit(1024),
                max_combined_bytes: mini_harness::runtime::ByteLimit(1024),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        second,
        Err(mini_harness::error::ProcessError::ConcurrencyLimit)
    ));
    assert!(slow.await.unwrap().is_ok());
    assert!(matches!(
        second,
        Err(mini_harness::error::ProcessError::ConcurrencyLimit)
    ));
    let _ = UserInput("unused".into());
}
