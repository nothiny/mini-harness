# 目录

[源码精读教材首页](tutorial/README.md)

# Part 1：理解 mini-harness

- [第 0 章：mini-harness 到底是什么？](tutorial/00-overview.md)
- [第 1 章：项目整体架构](tutorial/architecture.md)
- [第 2 章：第一次完整运行](tutorial/first-run.md)

# Part 2：理解 Harness Core

- [第 3 章：核心数据结构与身份](tutorial/01-ids-and-events.md)
- [第 4 章：Session 与生命周期](tutorial/03-mock-provider-and-session.md)
- [第 5 章：AgentLoop 的完整实现](tutorial/05-agent-loop.md)
- [第 6 章：Model Provider 与模型 API 适配](tutorial/08-openai-provider.md)
- [第 7 章：Tool 系统](tutorial/04-first-tool.md)
- [第 8 章：Executor 系统与副作用边界](tutorial/executor.md)

# Part 3：深入工程设计

- [第 9 章：Event Sourcing、EventStore 与 Reducer](tutorial/02-event-store-and-reducer.md)
- [第 10 章：Session Actor 与并发](tutorial/session-actor.md)
- [第 11 章：进程管理、输出与取消](tutorial/06-process-cancellation.md)
- [第 12 章：崩溃恢复、Checkpoint 与未知副作用](tutorial/07-recovery.md)
- [第 13 章：Policy、审批与安全边界](tutorial/policy.md)

# Part 4：外围系统

- [第 14 章：CLI 与 JSONL 控制协议](tutorial/09-cli-and-protocol.md)
- [第 15 章：Rust TUI、Ink UI 与显示状态](tutorial/10-ui.md)
- [第 16 章：Scheduler、队列与资源限制](tutorial/11-scheduler.md)
- [第 17 章：配置、测试与 Benchmark](tutorial/config-tests-benchmarks.md)

# Part 5：真正掌握项目

- [第 18 章：三个完整源码调用链复盘](tutorial/call-chain-cases.md)
- [第 19 章：架构设计决策与取舍](tutorial/design-decisions.md)
- [第 20 章：如何扩展 mini-harness](tutorial/12-next-steps.md)

# 附录

- [术语与 Rust 阅读提示](tutorial/glossary.md)
- [源码研究与原教程审查](tutorial/source-audit.md)
- [验证记录与已知边界](tutorial/validation.md)
