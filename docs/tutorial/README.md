# mini-harness：从零开始的源码精读教材

本教材基于当前仓库提交 `1720acefc11fe38cfb9b3bb3d649832ef951d4a8`，核验日期 2026-10-14。目标读者了解LLM/Tool Calling并有编程基础，但不需要提前认识本项目或熟练Rust异步。读完应能从CLI追踪完整请求，解释各层边界，并独立规划、实现和调试小型扩展。

先按行为阅读，再追源码：模型只提出建议，Harness决定授权、执行、记账、取消和恢复。第一次使用固定Mock脚本，把模型质量和网络波动从工程机制中排除。

## 推荐阅读顺序

第一次阅读按第0—20章顺序；不要先钻进所有事件字段。第0—2章建立地图与真实运行，第3—8章解释闭环各角色，第9—13章深入可靠性边界，第14—17章看进程外控制与验证，第18—20章复盘并掌握扩展。

| 章 | 阅读目标 |
|---|---|
| [第 0 章：mini-harness 到底是什么？](00-overview.md) | Harness解决什么及尚未实现什么 |
| [第 1 章：项目整体架构](architecture.md) | 完整模块、依赖、控制流与数据流 |
| [第 2 章：第一次完整运行](first-run.md) | 真实11事件read Demo |
| [第 3 章：核心数据结构与身份](01-ids-and-events.md) | 身份、history、state与事件形状 |
| [第 4 章：Session 与生命周期](03-mock-provider-and-session.md) | 开始/完成/失败/审批/恢复 |
| [第 5 章：AgentLoop 的完整实现](05-agent-loop.md) | 逐分支主循环、预算、异步与记账 |
| [第 6 章：Model Provider 与模型 API 适配](08-openai-provider.md) | Mock、Responses、CC、HTTP与native IDs |
| [第 7 章：Tool 系统](04-first-tool.md) | read/edit/bash参数与结果 |
| [第 8 章：Executor 系统与副作用边界](executor.md) | 文件能力、hash/锁/rename与路径边界 |
| [第 9 章：Event Sourcing、EventStore 与 Reducer](02-event-store-and-reducer.md) | 逐事件演算、JSONL存储、replay |
| [第 10 章：Session Actor 与并发](session-actor.md) | 单写者、future借用、输入ack与队列 |
| [第 11 章：进程管理、输出与取消](06-process-cancellation.md) | 组、双流、KILL、timeout与取消 |
| [第 12 章：崩溃恢复、Checkpoint 与未知副作用](07-recovery.md) | 故障窗口、Unknown、恢复与快照验证 |
| [第 13 章：Policy、审批与安全边界](policy.md) | Allow/Ask/Deny与授权/隔离区别 |
| [第 14 章：CLI 与 JSONL 控制协议](09-cli-and-protocol.md) | 装配、版本、真实JSONL客户端 |
| [第 15 章：Rust TUI、Ink UI 与显示状态](10-ui.md) | Rust行/全屏与Ink消息/审批路径 |
| [第 16 章：Scheduler、队列与资源限制](11-scheduler.md) | Scheduler门面、FIFO与共享资源 |
| [第 17 章：配置、测试与 Benchmark](config-tests-benchmarks.md) | 默认/覆盖、测试证据与Benchmark边界 |
| [第 18 章：三个完整源码调用链复盘](call-chain-cases.md) | read、edit批准、bash取消/崩溃复盘 |
| [第 19 章：架构设计决策与取舍](design-decisions.md) | 背景、收益、成本与替代方案 |
| [第 20 章：如何扩展 mini-harness](12-next-steps.md) | 五类扩展的修改点、步骤与测试 |


如果已经看过项目，按问题复读：

- 调用链与模型闭环：2 → 5 → 6 → 7 → 8 → 18。
- 日志、并发与崩溃：3 → 9 → 10 → 11 → 12 → 18。
- 审批、协议与UI：4 → 13 → 14 → 15 → 16。
- 准备扩展：17 → 19 → 20，并回到相关模块的失败测试。

## 如何使用每一章

每章固定包含：本章问题、设计思想、架构位置、源码位置、按入口分析、可验证运行、设计取舍、常见误解、总结、3—5项自测及参考答案、下一章衔接。源码片段只用于定位；简化代码、wire示例与故障fixture均标明性质，不把它们当本次真实模型输出。

文件名保留原教程URL，所以文件数字不再等于新章号；章标题、导航和SUMMARY才是阅读顺序。例如原 `02-event-store-and-reducer.md` 现在是第9章，而第一次运行是 `first-run.md`。正文没有创建另一套平行教材。

## 第一个实验

在仓库根目录：

```bash
cargo test --locked
cargo run --locked -- demo read README.md --json
```

这是只读文件Demo，事件在内存，不建立磁盘会话日志。固定Mock先要求read，再返回`read complete`；默认`run "..."`的Mock只返回`mock response`，不会依据自然语言选择工具。编辑Demo必须按第7/18章放在临时workspace，不能拿仓库README做替换实验。

最重要的主链是：

```text
run_demo / run_cli_turn / protocol start_turn
 → Session::start_turn 或 SessionCommand::StartTurn
 → AgentLoop::begin_turn / begin_queued_turn
 → continue_turn_with_reason
 → ContextSnapshot → ModelRequest → sample_model → ModelProvider
 → ModelCompletion → continuation/model/request事件
 → Registry.lookup → ToolPolicy.decide
 → ToolStarted → Tool.execute → Executor
 → ToolCompleted → append → EventStore → reducer派生history
 → 下一次ModelRequest → 最终文本 → TurnCompleted
```

这是一条循环，append穿插在所有阶段，并非只在最终回答时保存日志。准确的时序和逐事件表见第2、5、9、18章。

## 阅读中必须区分的边界

| 两侧 | 区别 |
|---|---|
| Model / Harness | 建议与受控执行 |
| Session / Turn | 长期会话与单次工作 |
| Session / AgentLoop | 调用/生命周期门面与执行算法 |
| Provider / Executor | 网络模型协议与现实系统能力 |
| Tool / Executor | 模型参数语义与文件/进程机制 |
| Event / State | 历史事实与推导视图 |
| EventStore / Checkpoint | 权威日志与可丢弃快照 |
| Actor / async task | 状态单写者与并发等待/工作任务 |
| Cancel / Timeout | 外部控制信号与预算触发，具体终态可能不同 |
| Policy / Sandbox | 允许开始与OS物理隔离，当前无sandbox |
| Execution Success / Task Correctness | 合法执行终态与独立验证任务满足要求 |

## 证据、版本与限制

所有源码链接指向上述固定提交，显示相对仓库路径；跨章链接是本地Markdown，适合仓库和mdBook。发生实现差异时以该提交源码及测试为准。原README、旧注释和教程的重要差异集中在[审查记录](source-audit.md)。

本次189个Rust测试通过，两个离线Demo、JSONL往返及编辑审批fixture已实测，文档引用/导航、图语法和mdBook构建的详情见[验证记录](validation.md)。真实模型API、外部业务Benchmark、Linux/Windows平台运行和真人交互终端不在本次实测范围。

Mermaid代码块可在支持Mermaid的Markdown阅读器渲染；当前mdBook配置没有Mermaid渲染插件，因此HTML构建保留图的代码文本。本次校验图语法与源调用关系，不声称mdBook自动绘图。源码和教程仍可完整阅读。

[术语与Rust阅读提示](glossary.md)解释Arc/Mutex/await/select/channel等首次阅读障碍；[完整目录树](architecture.md)帮助定位；[验证记录](validation.md)说明命令证明的范围。
