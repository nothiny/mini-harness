//! 示例 4: 理解崩溃恢复
//!
//! 运行: cargo run --example 04_crash_recovery
//!
//! 核心: 进程崩溃时事件日志完好无损。恢复 = 从日志完整重放。
//! 日志末尾一个仍在运行的 turn 表示"崩溃发生在 turn 中间"。

use mini_harness::{
    durable::{Event, EventPayload, EventStore, JsonlEventStore, reduce},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse},
    policy::DefaultPolicy,
    runtime::{EventSeq, SessionId, SessionState, TurnId, UserInput, session::Session},
    tools::ToolRegistry,
};
use std::sync::Arc;

fn numbered(mut event: Event, seq: u64) -> Event {
    event.seq = EventSeq(seq);
    event
}

#[tokio::main]
async fn main() {
    let log_dir = std::env::temp_dir().join("mini-harness-example-recovery");
    tokio::fs::create_dir_all(&log_dir).await.unwrap();
    let log = log_dir.join("events.jsonl");
    let _ = tokio::fs::remove_file(&log).await;

    // 1. 模拟崩溃: 日志写了一半 —— turn 已开始但没有终止事件
    let session = SessionId::new();
    let turn = TurnId::new();
    let store = JsonlEventStore::new(log.clone());
    for (seq, (turn_id, payload)) in [
        (None, EventPayload::SessionCreated),
        (
            None,
            EventPayload::UserInputRecorded {
                input: UserInput("崩溃前的输入".into()),
            },
        ),
        (Some(turn), EventPayload::TurnStarted),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append(numbered(
                Event::new(session, turn_id, payload),
                (seq + 1) as u64,
            ))
            .await
            .unwrap();
    }
    drop(store);
    println!("模拟崩溃: 日志有 {} 个事件，turn 未终止\n", 3);

    // 2. 恢复 = 重放: 状态重建到崩溃前的最后一刻
    let events = JsonlEventStore::new(log.clone())
        .read_from(EventSeq(1))
        .await
        .unwrap();
    let mut state = SessionState::new(session);
    for event in &events {
        reduce(&mut state, event).unwrap();
    }
    println!("重放结果:");
    println!("  last_seq = {}", state.last_seq.0);
    println!(
        "  活跃 turn: {}（崩溃发生在 turn 中间）",
        state
            .active_turn
            .as_ref()
            .map(|t| t.turn_id.to_string())
            .unwrap_or_else(|| "无".into())
    );
    println!("  历史记录: {} 条\n", state.history.len());

    // 3. 用 Session::resume 接管: 放弃未完成的 turn，继续新对话
    let store = Arc::new(JsonlEventStore::new(log));
    let mut resumed = Session::resume_with_policy(
        Arc::new(MockProvider::new(vec![MockResponse::Text(
            "恢复后的第一条回答。".into(),
        )])),
        Arc::new(LocalExecutor::new(log_dir.clone())),
        Arc::new(ToolRegistry::default()),
        store,
        session,
        Arc::new(DefaultPolicy),
        Default::default(),
    )
    .await
    .unwrap();

    // 未完成的 turn 先显式取消（恢复策略由调用方决定）
    if let Some(active) = resumed.state().active_turn.clone() {
        resumed.cancel_turn(active.turn_id).await.unwrap();
        println!("已取消崩溃前的 turn {}\n", active.turn_id);
    }

    let (_, text) = resumed.start_turn("崩溃恢复后继续".into()).await.unwrap();
    println!("新 turn 完成: {text}\n");

    println!("关键点:");
    println!("  1. 恢复不需要额外数据: 事件日志是唯一事实");
    println!("  2. 末尾活跃 turn = 崩溃点，处置（取消/继续）由调用方决定");
    println!("  3. 重放是确定性的: 任何进程重放同一日志得到同一状态");
}
