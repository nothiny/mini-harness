//! 示例 2: 理解 Agent Loop
//!
//! 运行: cargo run --example 02_agent_loop
//!
//! 核心: sample → tool calls → results → next step，直到模型给出
//! 最终文本或达到步数上限。全程离线（MockProvider）。

use mini_harness::{
    durable::{EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    policy::AllowAllPolicy,
    runtime::{EventSeq, SessionId, ToolCallId, ToolName, UserInput, session::Session},
    tools::{ReadTool, ToolRegistry},
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let workspace = std::env::temp_dir();
    let demo_file = workspace.join("mini-harness-example.txt");
    tokio::fs::write(&demo_file, "hello from the example\n")
        .await
        .unwrap();

    // 1. 脚本化的模型: 先调用 read 工具，再给出最终回答
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(ToolCall {
            call_id: ToolCallId::new(),
            name: ToolName("read".into()),
            input: serde_json::json!({"path": demo_file.to_string_lossy()}),
        }),
        MockResponse::Text("我读到了示例文件的内容。".into()),
    ]));

    // 2. 工具注册表 + 执行器 + 内存事件日志
    let mut registry = ToolRegistry::default();
    registry.register(ReadTool { max_bytes: 1024 }).unwrap();
    let store = Arc::new(InMemoryEventStore::default());

    // 3. 一个同步 Session: 一次只跑一个 turn
    let mut session = Session::new_with_policy(
        provider,
        Arc::new(LocalExecutor::new(workspace.clone())),
        Arc::new(registry),
        store.clone(),
        Arc::new(AllowAllPolicy),
    );
    println!(
        "session: {}",
        SessionId::to_string(&session.state().session_id)
    );
    println!("开始 turn…\n");

    let (turn_id, output) = session
        .start_turn("读取示例文件".into())
        .await
        .expect("turn completes");

    println!("turn {turn_id} 完成:");
    println!("  最终回答: {output}\n");

    // 4. 事件日志是唯一事实: 每个 turn 的事实一目了然
    let events = store.read_from(EventSeq(1)).await.unwrap();
    println!("生成的事件（{} 个）:", events.len());
    for event in &events {
        println!("  {:>3}  {}", event.seq.0, event.payload.kind());
    }

    // 5. 模型在第二轮看到了工具结果（上下文累积）
    println!("\n关键点:");
    println!("  1. 循环: sample → tool call → result → sample → final text");
    println!("  2. 工具结果进入 history，下一次采样对模型可见");
    println!("  3. 事件日志完整记录了这次循环的每一步");
    let _ = UserInput("docs".into());
}
