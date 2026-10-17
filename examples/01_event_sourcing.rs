//! 示例 1: 理解事件溯源
//!
//! 运行: cargo run --example 01_event_sourcing
//!
//! 核心: 状态 = fold(events)。同一事件序列重放任意次，得到相同的状态。

use mini_harness::{
    durable::{Event, EventPayload, ToolOutcome, reduce},
    runtime::{
        ModelText, SessionId, SessionState, ToolCallId, ToolName, ToolResult, TurnId, UserInput,
    },
};

fn numbered(mut event: Event, seq: u64) -> Event {
    event.seq = mini_harness::runtime::EventSeq(seq);
    event
}

fn main() {
    let session = SessionId::new();
    let turn = TurnId::new();
    let call = ToolCallId::new();

    // 一次完整 turn 的事实序列（与真实 agent loop 写入的完全一致）
    let events = vec![
        numbered(Event::new(session, None, EventPayload::SessionCreated), 1),
        numbered(
            Event::new(
                session,
                None,
                EventPayload::UserInputRecorded {
                    input: UserInput("read the config".into()),
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
                EventPayload::ModelResponseRecorded {
                    text: ModelText("tool call: read".into()),
                },
            ),
            4,
        ),
        numbered(
            Event::new(
                session,
                Some(turn),
                EventPayload::ToolRequested {
                    call_id: call,
                    name: ToolName("read".into()),
                    input: serde_json::json!({"path": "config.toml"}),
                    requires_approval: false,
                },
            ),
            5,
        ),
        numbered(
            Event::new(
                session,
                Some(turn),
                EventPayload::ToolCompleted {
                    call_id: call,
                    outcome: ToolOutcome::Succeeded {
                        result: ToolResult("file contents…".into()),
                    },
                    duration_ms: 3,
                },
            ),
            6,
        ),
        numbered(
            Event::new(
                session,
                Some(turn),
                EventPayload::TurnCompleted {
                    text: ModelText("here is the config".into()),
                },
            ),
            7,
        ),
    ];

    // 从事件重建状态 —— 重放两次，状态必然相同
    let replay = || {
        events
            .iter()
            .fold(SessionState::new(session), |mut state, event| {
                reduce(&mut state, event).expect("合法事件序列");
                state
            })
    };
    let first = replay();
    let second = replay();

    println!("重建后的状态:");
    println!("  事件序号: {}", first.last_seq.0);
    println!("  会话状态: {:?}", first.status);
    println!("  历史记录数: {}", first.history.len());
    println!(
        "  活跃 Turn: {:?}",
        first
            .active_turn
            .as_ref()
            .map(|t| format!("{:?}", t.status))
    );
    println!();

    println!("关键点:");
    println!(
        "  1. 状态完全由事件决定: 重放两次结果相同 = {}",
        first == second
    );
    println!("  2. turn 结束后 active_turn 归 None（状态机闭合）");
    println!("  3. 不依赖外部输入（时间、随机数等）—— 全部确定性");
    println!();

    // 反例: 非法转换直接失败，而不是"修复"日志
    let mut state = SessionState::new(session);
    for event in &events {
        reduce(&mut state, event).unwrap();
    }
    let invalid = numbered(
        Event::new(
            session,
            Some(turn),
            EventPayload::TurnCompleted {
                text: ModelText("duplicate completion".into()),
            },
        ),
        8,
    );
    let result = reduce(&mut state, &invalid);
    println!(
        "4. 非法转换（重复完成同一 turn）被拒绝: {}",
        result.is_err()
    );
}
