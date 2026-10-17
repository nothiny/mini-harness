//! mini-harness: 一个用于学习的 Agent Harness 运行时。
//!
//! 核心概念（按依赖顺序阅读）:
//!
//! 1. [`durable`] — 事件溯源: JSONL 事件日志 + 确定性 reducer
//! 2. [`model`]   — 模型提供方抽象（Mock / OpenAI / DeepSeek）
//! 3. [`tools`]   — 工具注册表（read / edit / bash）
//! 4. [`policy`]  — 工具审批策略（allow / ask）
//! 5. [`runtime`] — Agent Loop 与 Session（模型-工具循环）
//! 6. [`executor`] — 进程执行（进程组、输出限制、取消）

pub mod config;
pub mod durable;
pub mod error;
pub mod executor;
pub mod model;
pub mod policy;
pub mod runtime;
pub mod tools;
pub use error::*;
#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
