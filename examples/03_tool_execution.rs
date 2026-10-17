//! 示例 3: 理解工具执行与审批
//!
//! 运行: cargo run --example 03_tool_execution
//!
//! 核心: `ask` 策略的工具在执行前暂停 turn（ApprovalPending），
//! 用户决定被记录为事件，拒绝即取消，批准则继续执行。

use mini_harness::{
    durable::{EventPayload, EventStore, InMemoryEventStore},
    executor::LocalExecutor,
    model::{MockProvider, MockResponse, ToolCall},
    policy::DefaultPolicy,
    runtime::{EventSeq, ToolCallId, ToolName, session::Session},
    tools::{BashTool, ToolRegistry},
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let workspace = std::env::temp_dir().join("mini-harness-example-tools");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let marker = workspace.join("approved-marker");

    // 1. 模型请求执行一个非只读 bash 命令 → DefaultPolicy 判为 Ask
    let call_id = ToolCallId::new();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall(ToolCall {
            call_id,
            name: ToolName("bash".into()),
            input: serde_json::json!({
                "command": format!("echo approved > '{}'", marker.to_string_lossy())
            }),
        }),
        MockResponse::Text("命令已执行。".into()),
    ]));

    let mut registry = ToolRegistry::default();
    registry.register(BashTool::default()).unwrap();
    let store = Arc::new(InMemoryEventStore::default());
    let mut session = Session::new_with_policy(
        provider,
        Arc::new(LocalExecutor::new(workspace.clone())),
        Arc::new(registry),
        store.clone(),
        Arc::new(DefaultPolicy),
    );

    // 2. 第一次尝试: turn 因等待审批而暂停
    let error = session
        .start_turn("运行命令".into())
        .await
        .expect_err("approval pending");
    println!("turn 暂停: {}", error);
    println!("副作用尚未发生: {}", !marker.exists());
    let turn_id = session.state().active_turn.as_ref().unwrap().turn_id;

    // 3. 用户批准 → 工具执行 → turn 继续直到完成
    let (turn, text) = session.approve_tool(turn_id, call_id, true).await.unwrap();
    println!("\n批准后 turn {} 完成: {}", turn, text);
    println!("副作用已发生: {}", marker.exists());

    // 4. 事件序列展示了完整的审批协议
    let events = store.read_from(EventSeq(1)).await.unwrap();
    println!("\n事件序列（{} 个）:", events.len());
    for event in &events {
        let detail = match &event.payload {
            EventPayload::ToolRequested {
                requires_approval, ..
            } => format!("（requires_approval={}）", requires_approval),
            EventPayload::ToolApprovalResponded { approved, .. } => {
                format!("（approved={}）", approved)
            }
            EventPayload::ToolCompleted { duration_ms, .. } => format!("（{}ms）", duration_ms),
            _ => String::new(),
        };
        println!(
            "  {:>3}  {:<26} {}",
            event.seq.0,
            event.payload.kind(),
            detail
        );
    }

    println!("\n关键点:");
    println!("  1. 审批请求 = requires_approval=true 的 tool.requested 事件");
    println!("  2. 用户决定本身是 durable 事实（tool.approval.responded）");
    println!("  3. 拒绝路径: approved=false → turn.cancelled，工具永不执行");
}
