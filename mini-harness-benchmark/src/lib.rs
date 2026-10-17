//! mini-harness-benchmark: 独立的 Benchmark 运行器。
//!
//! 只负责编排（逐任务启动 harness turn、保留证据），不运行验证器、
//! 不了解具体的评测套件（Terminal-Bench 等）。外层运行器（例如
//! Harbor）准备 workspace、调用本工具，再用套件自己的验证器评估结果。

pub mod manifest;

pub use manifest::*;
