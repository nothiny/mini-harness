# 源码研究与原教程审查记录

本教材核对的是本地提交 `1720acefc11fe38cfb9b3bb3d649832ef951d4a8`，研究日期为 2026-10-14。只修改文档；仓库已有的三份未跟踪研究文稿不属于本次教材。这里记录写作前建立的地图，验证结果另见[验证记录](validation.md)。

## 研究范围与方法

检查所有已跟踪目录、`Cargo.toml`、README、原有 13 个主题章节、首页、术语表、mdBook 配置、检查脚本及两份 CI workflow。按行为阅读 runtime、durable、model、tools、executor、policy、protocol、scheduler、Rust UI 和 Ink 的实现，再以 tests 和模块内测试核对输入、事件、返回值与异常分支。没有把未存在的设计稿或历史 checkout 当成实现依据。

源码注释多次引用 `docs/design.md` 与 `docs/implementation-plan.md`，但这两个文件在当前提交中不存在。它们不能作为本教材的证据；对应说明都重新从实现与测试核对。当前仓库也没有 benchmark runner、数据集、Harbor 适配器或 `benches/`，见第 17 章的可执行测量与外部评估边界。

## 写作前建立的模块地图

| 问题 | 实现入口 | 校验入口 |
|---|---|---|
| 一次离线请求 | `run_demo → Session::start_turn → AgentLoop::run_turn` | `src/lib_tests.rs::full_mock_read_turn` |
| 模型闭环与上下文 | `continue_turn_with_reason → ContextSnapshot → sample_model` | `tests/context_snapshot.rs`、`tests/agent_loop_limits.rs` |
| 工具与副作用 | `lookup → decide → execute → Executor` | `tests/read_tool.rs`、`tests/edit_tool.rs`、`tests/bash_tool.rs` |
| append 一致性 | `AgentLoop::append → reduce → EventStore::append` | `src/lib_tests.rs`、`tests/jsonl_cross_process.rs` |
| Actor 命令与队列 | `run_actor → run_active_turn/run_approved_turn → drain_queued_turns` | `tests/actor_queue_edges.rs`、`tests/scheduler.rs` |
| 恢复 | `classify_events/classify_state → recover_store` | `tests/recovery_classify.rs`、`tests/recovery_cli.rs` |
| 快照 | `create_checkpoint → write_atomic → replay_from_checkpoint` | `tests/checkpoint.rs`、`src/durable/checkpoint.rs` 模块内测试 |
| HTTP 适配 | `complete_request → execute_attempt → openai_protocol` | `tests/openai_provider.rs`、`tests/provider_contract.rs` |
| 协议与显示 | `run_stdio → handle_request` 与 `event_pump → project_event` | `tests/stdio_protocol.rs`、`tests/protocol_compat.rs`、`tests/tui_line_mode.rs` |

这些位置均可从[整体架构](architecture.md)和各章的相对源码链接进入。

## 原教程的优点与缺口

| 原内容 | 可保留的教学价值 | 本次改进 |
|---|---|---|
| `00-overview.md` 与首页 | 极简 loop、read demo、六条边界清楚 | 拆出完整架构和第一次运行，先行为后类型；固定版本与源码链接 |
| `01-ids-and-events.md` | newtype、事件信封解释直观 | 加入实际不存在的 `Message/Step/ToolExecution` 类型辨析与生命周期 |
| `02-event-store-and-reducer.md` | 验证—持久化—提交思路正确 | 增加完整逐事件演算、JSONL 锁/缓存/失败窗口；区分 store 与 runtime 校验 |
| `03-mock-provider-and-session.md` | 顺序 facade 与 Actor 对比有用 | Session 独立成章，Actor 另章解释 future 可变借用、延迟 ack、恢复队列限制 |
| `04-first-tool.md` | read 路径和 ToolContext 简明 | 深入 read 行窗口、edit 原子替换、bash 结果；Executor/Policy 分章 |
| `05-agent-loop.md` | 主循环伪代码有帮助 | 增加准确方法链、metadata observer、状态图、批次 defer、分支错误表和取消收尾 |
| `06-process-cancellation.md` | 并发排空和三层字节限额清楚 | 修正取消/超时终态的条件；移除会改仓库 README 的编辑示例；说明 SIGKILL 与进程组边界 |
| `07-recovery.md` | Unknown 与自动重试边界清楚 | 修正 Checkpoint 跳过前缀的过强描述；细化 repair、幂等与跨进程终态盲区 |
| `08-openai-provider.md` | 统一内部数据与两种 wire 对比有用 | 深入请求/响应字段、native ID、attempt ack；列出 CC override/流式限制 |
| `09-cli-and-protocol.md` | stdout 分离、v1/v2 投影解释有用 | 增加可执行 JSONL 客户端与真实消息、串行请求阻塞边界 |
| `10-ui.md` | Ink 三层职责清楚 | 补 Rust 全屏、行模式、split 客户端；明确两种 Ctrl-C 和服务器随 UI 退出的行为 |
| `11-scheduler.md` | FIFO、executor semaphore 分工正确 | 明确共享范围、SchedulerConfig 未接线、进程重启不能自动续跑已确认 Turn ID |
| `12-next-steps.md` | 收束架构主题有用 | 变成第 20 章的五类扩展指南；增加第 18/19 章案例和权衡 |

原章节大多没有统一的源码定位、完整时序图、逐步状态说明和思考题答案。本次每章使用 11 项模板，并保持原文件 URL 可继续访问。

## README/注释与当前实现差异

| 表述 | 当前证据与教材采用的解释 |
|---|---|
| README TOML 列出 `session.max_queued_inputs` | `src/config.rs::SessionConfig` 无此字段且拒绝未知字段；只能从 `AgentLoopConfig` 设置，CLI 默认 16 |
| README 说 doctor 检查 DeepSeek key | `src/main.rs::run_doctor` 只有 mock/openai 分支；deepseek 在这里报 unknown provider，尽管运行时支持它 |
| README 架构把所有 CLI 经 JSONL/Actor | `run`、`resume`、`demo` 使用直接 `Session`；`serve` 与 UI 才经 Actor |
| 进程组连所有子孙一起清理 | `kill(-pid, SIGKILL)` 只覆盖当时属于该组的进程；`setsid`/换组者不受保证 |
| 安全投影不包含工具输入 | v2 不带完整 payload，但 `input_summary` 会显示前 100 字符命令或路径；不是通用秘密清洗器 |
| 原教程称 Checkpoint 跳过日志前缀 | `replay_from_checkpoint` 读取完整日志、reduce 前缀、比较快照，再应用后缀；不能承诺冷启动复杂度下降 |
| `Tool` 注释称错误 handed back to model | `Err(String)` 在主循环变成 `tool.failed` 和 `turn.failed`，不进入下一次 Tool history |
| DefaultPolicy 注释称只读命令不会修改状态 | 仅按少量字符和前两个单词判断，不能证明任意参数安全；正常 CLI 用 ConfiguredPolicy，默认 bash Ask |
| `OpenAiProvider::deepseek` 旧注释说 reasoning 丢弃 | CC 非流式解析实际保留 `reasoning_content` 并回传；流式仍不积累该字段 |
| 接口注释称 per-request model override 通用 | Responses 使用 `request.model`；`build_cc_request_body` 使用 `config.model` |
| Input durable 等于自动重启队列执行 | 日志保存文本和 FIFO；Actor 的预分配 Turn ID、queue 和 waiters 不从日志重建，恢复不会自动 drain |

以上是阅读时必须分辨的实现边界，不是本次业务修改建议。

返回[教材首页](README.md)。
