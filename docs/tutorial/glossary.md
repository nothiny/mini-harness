# 术语表

按主题整理。每条给出最短定义和对应的源码位置；读教程遇到不熟的词，先回这里查。

## 运行时结构

| 术语 | 含义 | 相关位置 |
|---|---|---|
| Harness | 模型循环之外的全部系统：授权、执行、持久化、恢复、协议与 UI | 第 0 章 |
| Session | 可恢复的长期会话，包含历史、配置和多个 turn | `src/runtime/session.rs` |
| Turn | 一次用户输入到最终回答、失败、取消或暂停 | `src/runtime/state.rs` |
| Step | 一次模型请求及其结果处理，一个 turn 可以包含多个 step | `src/runtime/agent_loop.rs` |
| ToolExecution | 一个具体工具调用实例，有独立 ID 和执行状态 | `src/runtime/state.rs` |
| ContextSnapshot | 拥有所有权、不可变的模型输入窗口，负责预算截断 | `src/runtime/context.rs` |
| 事实（fact） | 已经发生的事情，只追加、不修改，进入 event log | `src/durable/event.rs` |
| 派生状态（derived state） | reducer 从事实计算出来的当前视图 | `src/durable/reducer.rs` |

## 模型与 Provider

| 术语 | 含义 | 相关位置 |
|---|---|---|
| Provider | 把模型服务差异翻译成 runtime 能理解的请求和响应 | `src/model/provider.rs` |
| ModelRequest | 一次模型请求的拥有所有权快照（历史、工具、continuation） | `src/model/provider.rs` |
| ModelResponse | 归一化后的模型结果：文本、单个或多个工具调用 | `src/model/provider.rs` |
| MockProvider | 按脚本返回确定性响应的 provider，也是核心设计工具 | `src/model/mock.rs` |
| ProviderContinuation | provider 自己维护的续传数据，重启后仍可继续响应链 | `src/runtime/state.rs` |
| Wire style | 线上格式；项目同时支持 Responses 与 Chat Completions | `src/model/openai_protocol.rs` |

## 工具、策略与执行

| 术语 | 含义 | 相关位置 |
|---|---|---|
| Tool / ToolSpec / ToolContext | 工具声明、给模型的 schema、执行时的最小能力集 | `src/tools/spec.rs` |
| RiskClass | 工具风险等级：Read / Write / Execute | `src/tools/spec.rs` |
| PolicyDecision | 授权决策：Allow（直接执行）/ Ask（先审批）；拒绝统一表示为「Ask + 用户拒绝」 | `src/policy/mod.rs` |
| fail-closed | 无法判断风险时默认拒绝，而不是默认放行 | `src/policy/mod.rs` |
| Executor | 真正访问文件与进程的能力层 | `src/executor/trait.rs` |
| LocalExecutor | 本地文件与进程实现，未来可替换为 sandbox 或远程 | `src/executor/local.rs` |
| 进程组（process group） | 取消/超时时按进程组清理，避免遗留子孙进程 | `src/executor/process.rs` |
| 三层输出上限 | 分别限制 stdout、stderr 和合计输出 | `src/executor/process.rs` |
| OutcomeUnknown | 执行可能已产生副作用，但日志没有可靠结果 | `src/durable/event.rs` |
| ToolOutcome | 工具结果的确定分类：succeeded / rejected（执行前拒绝，无副作用）/ failed（执行后失败） | `src/durable/event.rs` |

## 持久化与恢复

| 术语 | 含义 | 相关位置 |
|---|---|---|
| Event / EventPayload | 一条事实及其内容 | `src/durable/event.rs` |
| seq / event_id / schema_version | 会话内序号、事件自身 ID、事件格式版本 | `src/durable/event.rs` |
| Reducer | 确定性函数 `reduce(state, event)`，不接受外部输入 | `src/durable/reducer.rs` |
| append 事务 | 先验证、再持久化、后提交状态；追加前校验日志未被其他进程写过 | `src/runtime/agent_loop.rs` |
| 全量重放（无 checkpoint） | 崩溃恢复 = 从事件日志完整重放；简化版刻意不保存快照 | `src/runtime/agent_loop_setup.rs` |
| 时间旅行调试 | `inspect --until <seq>` 只重放前 N 条事件，查看任意历史时刻的状态 | `src/main.rs` |

## 取消与并发

| 术语 | 含义 | 相关位置 |
|---|---|---|
| CancellationToken | 随请求传播的取消信号，沿 turn → child → executor 传递 | `src/runtime/agent_loop.rs` |
| 单写者（single writer） | 同一时刻只有一个驱动者能推进 Session 并追加事件，由 `&mut` 借用强制 | `src/runtime/session.rs` |
| begin_turn / continue_turn | 拆分“记录输入拿到 id”与“驱动执行”，供协议服务器异步组合 | `src/runtime/session.rs` |
| pending_input 单槽 | 已记录但未被 turn.started 消费的输入，一次只允许一条 | `src/runtime/state.rs` |
| TurnAlreadyActive | turn 运行期间（或输入未消费时）新输入被拒绝的错误 | `src/error.rs` |
| 进程并发上限 | executor 层共享 semaphore，限制跨 session 的总进程数 | `src/executor/local.rs` |

## 协议与 UI

| 术语 | 含义 | 相关位置 |
|---|---|---|
| JSONL 协议 | stdin 请求、stdout 响应与事件、stderr 诊断，一行一个 JSON；独立 crate | `mini-harness-server/src/jsonl.rs` |
| response vs notification | 带 id 的命令回复，与无 id 的事件通知，必须分别路由 | `mini-harness-server/src/jsonl.rs` |
| 安全投影 | 只向客户端暴露白名单字段，隐藏工具原始输入输出 | `mini-harness-server/src/projection.rs` |
| input_summary / input_detail | 给审批界面看的工具输入摘要（单行 / 有界结构化预览，带脱敏） | `mini-harness-server/src/projection.rs` |
| Ink TUI | `ui/` 下的 TypeScript 协议客户端，UI 只消费事件 | `ui/src/client.ts` |

## 明确不做的事（非目标）

| 术语 | 含义 |
|---|---|
| exactly-once | 外部副作用不承诺恰好执行一次；宁可标记 `OutcomeUnknown` |
| turn 期间的新输入 | 简化版直接拒绝（`TurnAlreadyActive`），不做 durable 输入队列 |
| checkpoint | 不保存快照，崩溃后总是全量重放 |
| OS 级 sandbox | 路径检查不等于 sandbox；真实隔离是后续深度专题 |
