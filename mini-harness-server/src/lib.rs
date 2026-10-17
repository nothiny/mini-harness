//! mini-harness-server: mini-harness 的 JSONL 协议服务器（可选组件）。
//!
//! 主库刻意保持单线程同步的 `Session` 接口；需要并发控制（turn 运行中
//! 响应取消/审批）的客户端 —— 例如 TUI —— 使用本 crate。

mod jsonl;
mod projection;
mod transport;

pub use jsonl::run_stdio;
