#[cfg(test)]
mod mock_turn_e2e {
    use crate::durable::EventStore;
    use crate::runtime::HistoryItem;
    use crate::*;
    use std::sync::Arc;
    use tempfile::tempdir;
    #[tokio::test]
    async fn full_mock_read_turn() {
        let d = tempdir().unwrap();
        tokio::fs::write(d.path().join("a.txt"), "hello")
            .await
            .unwrap();
        let call = model::ToolCall {
            call_id: runtime::ToolCallId::new(),
            name: runtime::ToolName("read".into()),
            input: serde_json::json!({"path":"a.txt"}),
        };
        let provider = Arc::new(model::MockProvider::new(vec![
            model::MockResponse::ToolCall(call),
            model::MockResponse::Text("done".into()),
        ]));
        let mut reg = tools::ToolRegistry::default();
        reg.register(tools::ReadTool { max_bytes: 100 }).unwrap();
        let store = Arc::new(durable::InMemoryEventStore::default());
        let mut s = runtime::session::Session::new(
            provider.clone(),
            Arc::new(executor::LocalExecutor::new(d.path().into())),
            Arc::new(reg),
            store.clone(),
        );
        let (_, text) = s.start_turn("read it".into()).await.unwrap();
        assert_eq!(text, "done");
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].history.iter().any(|item| {
            matches!(
                item,
                HistoryItem::Tool { name, result, .. }
                    if name.0 == "read"
                        && result.0.contains("\"text\":\"hello\"")
            )
        }));
        assert_eq!(s.state().status, runtime::SessionStatus::Idle);
        assert!(s.state().active_turn.is_none());
        assert_eq!(
            s.state()
                .history
                .iter()
                .filter_map(|item| match item {
                    HistoryItem::Assistant(text) => Some(text.0.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["tool call: read", "done"]
        );
        let events = store.read_from(runtime::EventSeq(1)).await.unwrap();
        // 简化事件协议: 每个工具调用恰好 request + complete 两个事件。
        // 10 core facts = 6 turn facts + 2 tool facts + 2 continuation facts.
        assert_eq!(events.len(), 10);
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
                "provider.continuation.updated",
                "model.response.recorded",
                "tool.requested",
                "tool.completed",
                "provider.continuation.updated",
                "model.response.recorded",
                "turn.completed",
            ]
        );
    }
}

#[cfg(test)]
mod jsonl_replay {
    use crate::durable::EventStore;
    use crate::executor::Executor;
    use crate::*;
    use tempfile::tempdir;
    #[tokio::test]
    async fn jsonl_reopens_and_replays() {
        let d = tempdir().unwrap();
        let path = d.path().join("events.jsonl");
        let store = durable::JsonlEventStore::new(path.clone());
        let session = runtime::SessionId::new();
        store
            .append(durable::Event::new(
                session,
                None,
                durable::EventPayload::SessionCreated,
            ))
            .await
            .unwrap();
        store
            .append(durable::Event::new(
                session,
                None,
                durable::EventPayload::UserInputRecorded {
                    input: crate::runtime::UserInput("hi".into()),
                },
            ))
            .await
            .unwrap();
        drop(store);
        let reopened = durable::JsonlEventStore::new(path);
        let events = reopened.read_from(runtime::EventSeq(1)).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(reopened.last_seq().await.unwrap().0, 2);
    }
    #[tokio::test]
    async fn local_executor_rejects_escape() {
        let d = tempdir().unwrap();
        let executor = executor::LocalExecutor::new(d.path().into());
        let result = executor
            .read_file(executor::ReadFileRequest {
                path: "../outside".into(),
                max_bytes: runtime::ByteLimit(10),
            })
            .await;
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod types_events {
    use crate::durable::{Event, EventPayload, ToolOutcome};
    use crate::runtime::{EventSeq, ModelText, SessionId, ToolCallId, UserInput};
    use crate::*;

    #[test]
    fn ids_are_round_tripable_and_type_scoped() {
        let session = SessionId::new();
        let json = serde_json::to_string(&session).unwrap();
        let decoded: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(session, decoded);
        assert!(serde_json::from_str::<ToolCallId>(&json).is_err());
    }

    #[test]
    fn every_core_event_round_trips() {
        let session = SessionId::new();
        let turn = runtime::TurnId::new();
        let call = runtime::ToolCallId::new();
        let payloads = vec![
            EventPayload::SessionCreated,
            EventPayload::UserInputRecorded {
                input: UserInput("hello".into()),
            },
            EventPayload::TurnStarted,
            EventPayload::ModelResponseRecorded {
                text: ModelText("answer".into()),
            },
            EventPayload::ToolRequested {
                call_id: call,
                name: runtime::ToolName("read".into()),
                input: serde_json::json!({"path":"a"}),
                requires_approval: false,
            },
            EventPayload::ToolApprovalResponded {
                call_id: call,
                approved: true,
            },
            EventPayload::ToolCompleted {
                call_id: call,
                outcome: ToolOutcome::Succeeded {
                    result: runtime::ToolResult("ok".into()),
                },
                duration_ms: 12,
            },
            EventPayload::ToolCompleted {
                call_id: call,
                outcome: ToolOutcome::Rejected {
                    error: crate::error::ToolError::from("rejected"),
                },
                duration_ms: 0,
            },
            EventPayload::ToolOutcomeUnknown {
                call_id: call,
                reason: "interrupted".into(),
            },
            EventPayload::TurnCompleted {
                text: ModelText("done".into()),
            },
            EventPayload::TurnFailed {
                error: "failed".into(),
            },
            EventPayload::TurnCancelled,
        ];
        for payload in payloads {
            let mut event = Event::new(session, Some(turn), payload);
            event.seq = EventSeq(1);
            let json = serde_json::to_string(&event).unwrap();
            let decoded = Event::from_json(&json).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), json);
            assert!(!json.contains("OPENAI_API_KEY"));
        }
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let mut value = serde_json::to_value(Event::new(
            SessionId::new(),
            None,
            EventPayload::SessionCreated,
        ))
        .unwrap();
        value["schema_version"] = serde_json::json!(2);
        assert!(Event::from_json(&value.to_string()).is_err());
        value["schema_version"] = serde_json::json!(999);
        let error = Event::from_json(&value.to_string()).unwrap_err();
        assert!(matches!(error, error::DurableError::UnsupportedSchema(999)));
    }
}

#[cfg(test)]
mod error_types {
    use crate::error::ExecutionError;
    use crate::runtime::{ExecutionState, ToolResult};
    #[test]
    fn execution_state_preserves_typed_failure_and_unknown() {
        let failed = ExecutionState::Failed {
            error: ExecutionError::Timeout {
                message: "deadline exceeded".into(),
            },
        };
        let json = serde_json::to_string(&failed).unwrap();
        assert_eq!(
            serde_json::from_str::<ExecutionState>(&json).unwrap(),
            failed
        );
        assert_eq!(
            ExecutionState::OutcomeUnknown,
            ExecutionState::OutcomeUnknown
        );
        let completed = ExecutionState::Completed {
            result: ToolResult("ok".into()),
        };
        assert!(matches!(completed, ExecutionState::Completed { .. }));
        assert!(!matches!(completed, ExecutionState::Pending));
    }
}

#[cfg(test)]
mod reducer_rules {
    use crate::durable::{Event, EventPayload, ToolOutcome, reduce};
    use crate::runtime::{
        EventSeq, ModelText, SessionId, ToolCallId, ToolName, ToolResult, TurnId, UserInput,
    };
    use crate::*;
    fn event(session: SessionId, turn: Option<TurnId>, seq: u64, payload: EventPayload) -> Event {
        let mut event = Event::new(session, turn, payload);
        event.seq = EventSeq(seq);
        event
    }
    #[test]
    fn reducer_rejects_completion_before_approval_and_duplicate_completion() {
        let session = SessionId::new();
        let turn = TurnId::new();
        let call = ToolCallId::new();
        let mut state = runtime::SessionState::new(session);
        let events = [
            event(session, None, 1, EventPayload::SessionCreated),
            event(
                session,
                None,
                2,
                EventPayload::UserInputRecorded {
                    input: UserInput("x".into()),
                },
            ),
            event(session, Some(turn), 3, EventPayload::TurnStarted),
            event(
                session,
                Some(turn),
                4,
                EventPayload::ToolRequested {
                    call_id: call,
                    name: ToolName("bash".into()),
                    input: serde_json::json!({}),
                    requires_approval: true,
                },
            ),
        ];
        for event in events.iter() {
            reduce(&mut state, event).unwrap();
        }
        // 审批还挂起时不能完成工具。
        let early = event(
            session,
            Some(turn),
            5,
            EventPayload::ToolCompleted {
                call_id: call,
                outcome: ToolOutcome::Succeeded {
                    result: ToolResult("x".into()),
                },
                duration_ms: 0,
            },
        );
        assert!(reduce(&mut state, &early).is_err());
        // 用户批准后才能完成。
        let approved = event(
            session,
            Some(turn),
            5,
            EventPayload::ToolApprovalResponded {
                call_id: call,
                approved: true,
            },
        );
        reduce(&mut state, &approved).unwrap();
        let completed = event(
            session,
            Some(turn),
            6,
            EventPayload::ToolCompleted {
                call_id: call,
                outcome: ToolOutcome::Succeeded {
                    result: ToolResult("x".into()),
                },
                duration_ms: 0,
            },
        );
        reduce(&mut state, &completed).unwrap();
        let duplicate = event(
            session,
            Some(turn),
            7,
            EventPayload::ToolCompleted {
                call_id: call,
                outcome: ToolOutcome::Succeeded {
                    result: ToolResult("x".into()),
                },
                duration_ms: 0,
            },
        );
        assert!(reduce(&mut state, &duplicate).is_err());
    }
    #[test]
    fn reducer_replay_is_deterministic() {
        let session = SessionId::new();
        let turn = TurnId::new();
        let events = vec![
            event(session, None, 1, EventPayload::SessionCreated),
            event(
                session,
                None,
                2,
                EventPayload::UserInputRecorded {
                    input: UserInput("x".into()),
                },
            ),
            event(session, Some(turn), 3, EventPayload::TurnStarted),
            event(
                session,
                Some(turn),
                4,
                EventPayload::ModelResponseRecorded {
                    text: ModelText("done".into()),
                },
            ),
            event(
                session,
                Some(turn),
                5,
                EventPayload::TurnCompleted {
                    text: ModelText("done".into()),
                },
            ),
        ];
        let mut a = runtime::SessionState::new(session);
        let mut b = runtime::SessionState::new(session);
        for event in &events {
            reduce(&mut a, event).unwrap();
            reduce(&mut b, event).unwrap();
        }
        assert_eq!(a, b);
        assert_eq!(a.pending_input, None);
    }
    #[test]
    fn reducer_rejects_input_while_a_turn_is_active() {
        let session = SessionId::new();
        let turn = TurnId::new();
        let mut state = runtime::SessionState::new(session);
        for event in [
            event(session, None, 1, EventPayload::SessionCreated),
            event(
                session,
                None,
                2,
                EventPayload::UserInputRecorded {
                    input: UserInput("x".into()),
                },
            ),
            event(session, Some(turn), 3, EventPayload::TurnStarted),
        ] {
            reduce(&mut state, &event).unwrap();
        }
        let queued = event(
            session,
            None,
            4,
            EventPayload::UserInputRecorded {
                input: UserInput("too early".into()),
            },
        );
        assert!(reduce(&mut state, &queued).is_err());
    }
}

#[cfg(test)]
mod jsonl_store {
    use crate::durable::{Event, EventPayload, EventStore, JsonlEventStore};
    use crate::*;
    use tempfile::tempdir;
    #[tokio::test]
    async fn jsonl_rejects_partial_tail() {
        let d = tempdir().unwrap();
        let path = d.path().join("events.jsonl");
        let event = serde_json::to_string(&Event::new(
            runtime::SessionId::new(),
            None,
            EventPayload::SessionCreated,
        ))
        .unwrap();
        tokio::fs::write(&path, format!("{event}\n{{\"schema_version\":1"))
            .await
            .unwrap();
        let error = JsonlEventStore::new(path).last_seq().await.unwrap_err();
        assert!(matches!(error, error::DurableError::Corrupt(2)));
    }

    #[tokio::test]
    async fn jsonl_accepts_an_existing_empty_file() {
        let d = tempdir().unwrap();
        let path = d.path().join("events.jsonl");
        tokio::fs::write(&path, b"").await.unwrap();
        let store = JsonlEventStore::new(path);
        assert_eq!(store.last_seq().await.unwrap(), runtime::EventSeq(0));
        assert!(
            store
                .read_from(runtime::EventSeq(1))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn jsonl_rejects_blank_lines_and_invalid_utf8() {
        let d = tempdir().unwrap();
        let path = d.path().join("events.jsonl");
        let session = runtime::SessionId::new();
        let mut first = Event::new(session, None, EventPayload::SessionCreated);
        first.seq = runtime::EventSeq(1);
        let event = serde_json::to_vec(&first).unwrap();
        tokio::fs::write(&path, [event.as_slice(), b"\n\n"].concat())
            .await
            .unwrap();
        assert!(matches!(
            JsonlEventStore::new(path.clone()).last_seq().await,
            Err(error::DurableError::Corrupt(2))
        ));
        tokio::fs::write(&path, [event.as_slice(), b"\n\xff\n"].concat())
            .await
            .unwrap();
        assert!(matches!(
            JsonlEventStore::new(path).last_seq().await,
            Err(error::DurableError::Corrupt(0))
        ));
    }
}

#[cfg(test)]
mod jsonl_regressions {
    use crate::durable::{Event, EventPayload, EventStore, JsonlEventStore, reduce};
    use crate::error::{DurableError, ToolError};
    use crate::runtime::{
        EventSeq, SessionId, SessionStatus, ToolCallId, ToolName, TurnId, UserInput,
    };
    use crate::*;
    use tempfile::tempdir;
    fn numbered(mut event: Event, seq: u64) -> Event {
        event.seq = EventSeq(seq);
        event
    }
    #[tokio::test]
    async fn jsonl_rejects_sequence_gaps_and_middle_corruption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let session = SessionId::new();
        let first = serde_json::to_string(&numbered(
            Event::new(session, None, EventPayload::SessionCreated),
            1,
        ))
        .unwrap();
        let gap = serde_json::to_string(&numbered(
            Event::new(
                session,
                None,
                EventPayload::UserInputRecorded {
                    input: UserInput("x".into()),
                },
            ),
            3,
        ))
        .unwrap();
        tokio::fs::write(&path, format!("{first}\n{gap}\n"))
            .await
            .unwrap();
        assert!(matches!(
            JsonlEventStore::new(path).last_seq().await,
            Err(DurableError::Corrupt(2))
        ));
        let path = dir.path().join("middle.jsonl");
        tokio::fs::write(&path, format!("{first}\nnot-json\n"))
            .await
            .unwrap();
        assert!(matches!(
            JsonlEventStore::new(path).last_seq().await,
            Err(DurableError::Corrupt(2))
        ));
    }
    #[test]
    fn reducer_allows_recovery_input_after_failure() {
        let session = SessionId::new();
        let mut state = runtime::SessionState::new(session);
        reduce(
            &mut state,
            &numbered(Event::new(session, None, EventPayload::SessionCreated), 1),
        )
        .unwrap();
        state.last_seq = EventSeq(1);
        state.status = SessionStatus::Failed;
        let mut input = Event::new(
            session,
            None,
            EventPayload::UserInputRecorded {
                input: UserInput("x".into()),
            },
        );
        input.seq = EventSeq(2);
        assert!(reduce(&mut state, &input).is_ok());
        assert_eq!(state.status, SessionStatus::Idle);
    }
    #[test]
    fn reducer_keeps_tool_error_type() {
        let session = SessionId::new();
        let turn = TurnId::new();
        let call = ToolCallId::new();
        let mut state = runtime::SessionState::new(session);
        let events = vec![
            numbered(Event::new(session, None, EventPayload::SessionCreated), 1),
            numbered(
                Event::new(
                    session,
                    None,
                    EventPayload::UserInputRecorded {
                        input: UserInput("x".into()),
                    },
                ),
                2,
            ),
            numbered(
                Event::new(session, Some(turn), EventPayload::TurnStarted),
                3,
            ),
            numbered(
                Event::new(
                    session,
                    Some(turn),
                    EventPayload::ToolRequested {
                        call_id: call,
                        name: ToolName("read".into()),
                        input: serde_json::json!({}),
                        requires_approval: false,
                    },
                ),
                4,
            ),
            numbered(
                Event::new(
                    session,
                    Some(turn),
                    EventPayload::ToolCompleted {
                        call_id: call,
                        outcome: crate::durable::ToolOutcome::Failed {
                            error: ToolError::from("permission denied"),
                        },
                        duration_ms: 0,
                    },
                ),
                5,
            ),
        ];
        for event in events {
            reduce(&mut state, &event).unwrap();
        }
        assert!(matches!(
            state.active_turn.as_ref().unwrap().executions.get(&call),
            Some(runtime::ExecutionState::Failed {
                error: crate::error::ExecutionError::Tool { .. }
            })
        ));
        // 执行后的失败可能已有副作用：阻塞 turn 完成。
        let done = numbered(
            Event::new(
                session,
                Some(turn),
                EventPayload::ModelResponseRecorded {
                    text: crate::runtime::ModelText("done".into()),
                },
            ),
            6,
        );
        reduce(&mut state, &done).unwrap();
        let completed = numbered(
            Event::new(
                session,
                Some(turn),
                EventPayload::TurnCompleted {
                    text: crate::runtime::ModelText("done".into()),
                },
            ),
            7,
        );
        assert!(reduce(&mut state, &completed).is_err());
    }
}

#[cfg(test)]
mod session_lifecycle {
    use crate::durable::EventStore;
    use crate::error::ProviderError;
    use crate::model::{MockProvider, MockResponse};
    use crate::runtime::SessionStatus;
    use crate::runtime::session::Session;
    use crate::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn executor(dir: &tempfile::TempDir) -> Arc<executor::LocalExecutor> {
        Arc::new(executor::LocalExecutor::new(dir.path().into()))
    }

    #[tokio::test]
    async fn provider_error_records_turn_failed() {
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::Error(ProviderError::from("upstream")),
            MockResponse::Text("recovered".into()),
        ]));
        let store = Arc::new(durable::InMemoryEventStore::default());
        let dir = tempdir().unwrap();
        let mut session = Session::new(
            provider,
            executor(&dir),
            Arc::new(tools::ToolRegistry::default()),
            store.clone(),
        );
        assert!(session.start_turn("hello".into()).await.is_err());
        assert_eq!(session.state().status, SessionStatus::Failed);
        assert_eq!(store.last_seq().await.unwrap().0, 4);
        let (_, text) = session
            .start_turn("retry after failure".into())
            .await
            .unwrap();
        assert_eq!(text, "recovered");
        assert_eq!(session.state().status, SessionStatus::Idle);
        assert!(store.last_seq().await.unwrap().0 > 4);
    }

    #[tokio::test]
    async fn mock_script_exhaustion_is_a_provider_error() {
        let provider = MockProvider::new(vec![]);
        let result = crate::model::ModelProvider::complete(
            &provider,
            crate::model::ModelRequest::new(vec![], vec![]),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Message { .. })));
    }

    /// 简化版没有 durable queue: turn 运行中（或挂起审批中）的新输入被
    /// 直接拒绝。
    #[tokio::test]
    async fn start_turn_is_rejected_while_another_turn_is_active() {
        let call = crate::model::ToolCall {
            call_id: runtime::ToolCallId::new(),
            name: runtime::ToolName("bash".into()),
            input: serde_json::json!({"command": "echo hi"}),
        };
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall(call),
            MockResponse::Text("done".into()),
        ]));
        let dir = tempdir().unwrap();
        let mut registry = tools::ToolRegistry::default();
        registry.register(tools::BashTool::default()).unwrap();
        let mut session = Session::new_with_policy(
            provider,
            executor(&dir),
            Arc::new(registry),
            Arc::new(durable::InMemoryEventStore::default()),
            Arc::new(policy::DefaultPolicy),
        );
        // bash `echo hi` 是只读命令 → Allow，turn 正常完成。
        session.start_turn("first".into()).await.unwrap();
        // 人为构造挂起状态: edit 工具需要审批。
        let edit_call = crate::model::ToolCall {
            call_id: runtime::ToolCallId::new(),
            name: runtime::ToolName("edit".into()),
            input: serde_json::json!({
                "path": "a.txt",
                "old_text": "before",
                "new_text": "after"
            }),
        };
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall(edit_call),
            MockResponse::Text("done".into()),
        ]));
        let mut registry = tools::ToolRegistry::default();
        registry.register(tools::EditTool).unwrap();
        let mut session = Session::new_with_policy(
            provider,
            executor(&dir),
            Arc::new(registry),
            Arc::new(durable::InMemoryEventStore::default()),
            Arc::new(policy::DefaultPolicy),
        );
        tokio::fs::write(dir.path().join("a.txt"), "before")
            .await
            .unwrap();
        let error = session.start_turn("edit it".into()).await.unwrap_err();
        assert!(matches!(error, HarnessError::ApprovalPending(_)));
        assert!(session.state().active_turn.is_some());
        let rejected = session.start_turn("second".into()).await.unwrap_err();
        assert!(matches!(rejected, HarnessError::TurnAlreadyActive));
    }
}

#[cfg(test)]
mod turn_lifecycle {
    use crate::durable::{EventPayload, EventStore};
    use crate::model::{MockProvider, MockResponse, ToolCall};
    use crate::runtime::{SessionStatus, ToolCallId, ToolName};
    use crate::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    #[tokio::test]
    async fn turn_timeout_records_cancelled_without_completion() {
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::Delay(Duration::from_secs(5)),
            MockResponse::Text("late".into()),
        ]));
        let store = Arc::new(durable::InMemoryEventStore::default());
        let dir = tempdir().unwrap();
        let mut session = runtime::session::Session::new(
            provider,
            Arc::new(executor::LocalExecutor::new(dir.path().into())),
            Arc::new(tools::ToolRegistry::default()),
            store.clone(),
        );
        let error = session
            .start_turn_with_timeout("cancel me".into(), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(error, HarnessError::Timeout));
        let events = store.read_from(runtime::EventSeq(1)).await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnTimedOut))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnCompleted { .. }))
        );
    }
    #[tokio::test]
    async fn tool_failure_records_terminal_turn_failure() {
        let call = ToolCall {
            call_id: ToolCallId::new(),
            name: ToolName("missing".into()),
            input: serde_json::json!({}),
        };
        let provider = Arc::new(MockProvider::new(vec![MockResponse::ToolCall(call)]));
        let store = Arc::new(durable::InMemoryEventStore::default());
        let dir = tempdir().unwrap();
        let mut session = runtime::session::Session::new(
            provider,
            Arc::new(executor::LocalExecutor::new(dir.path().into())),
            Arc::new(tools::ToolRegistry::default()),
            store.clone(),
        );
        assert!(session.start_turn("invoke missing".into()).await.is_err());
        assert_eq!(session.state().status, SessionStatus::Failed);
        let events = store.read_from(runtime::EventSeq(1)).await.unwrap();
        // 未知工具在执行前被拒绝（Rejected），turn 因脚本耗尽而失败。
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ToolCompleted {
                outcome: durable::ToolOutcome::Rejected { .. },
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnFailed { .. }))
        );
    }
}
