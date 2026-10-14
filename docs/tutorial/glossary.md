# 术语与 Rust 源码阅读提示

本表为正文复读入口，不替代执行故事。所有名词以提交 `1720acefc11fe38cfb9b3bb3d649832ef951d4a8` 为准。

## 系统与事实

| 名词 | 当前实现含义 | 阅读位置 |
|---|---|---|
| Harness | 模型之外的授权、执行、预算、持久化、取消、恢复与客户端边界 | [第0章](00-overview.md) |
| Session / Turn | 长期会话 / 一次输入对应的工作；终态或审批暂停需分别判断 | [第4章](03-mock-provider-and-session.md) |
| Step | 一次采样与响应处理；当前是计数，没有持久化Step struct | [第3章](01-ids-and-events.md)、[第5章](05-agent-loop.md) |
| Message / ToolExecution | 教学概念；源码分别以HistoryItem与ExecutionState/TurnState表表达，无同名struct | [第3章](01-ids-and-events.md) |
| Event / EventPayload | 事实信封（ID/seq/time/session/turn） / 具体事实内容 | [第9章](02-event-store-and-reducer.md) |
| State / Reducer | 派生视图 / 纯粹按事件检查并更新状态的函数reduce | [第9章](02-event-store-and-reducer.md) |
| EventId / EventSeq | 唯一事实身份 / 会话内连续顺序；不可互代 | [第3章](01-ids-and-events.md) |
| Checkpoint | 可丢弃的派生快照；当前恢复仍重放前缀校验，不承诺O(suffix)启动 | [第12章](07-recovery.md) |
| OutcomeUnknown | 已进入副作用窗口却无可靠结果；不等于确定失败或确定成功 | [第12章](07-recovery.md) |
| RecoveryPolicy | RetryRead/InspectEditHash/RequireUserDecision建议，recover不自动执行它 | [第12章](07-recovery.md) |
| exactly-once | 副作用恰好一次的保证；当前文件/命令与日志没有联合事务，不提供此保证 | [第19章](design-decisions.md) |

## 能力与控制

| 名词 | 当前含义 | 阅读位置 |
|---|---|---|
| Provider / WireStyle | 统一模型接口 / Responses或Chat Completions线上格式 | [第6章](08-openai-provider.md) |
| Continuation | response链/cursor/native call映射与可选reasoning元数据 | [第6章](08-openai-provider.md) |
| ContextSnapshot | 拥有数据的模型输入窗口；截断不修改日志 | [第5章](05-agent-loop.md) |
| Tool / Spec / Context | 操作语义 / 模型schema与risk / 当次执行能力和控制参数 | [第7章](04-first-tool.md) |
| Executor | 文件/进程能力与类型化结果；可信代码约定不是OS sandbox | [第8章](executor.md) |
| Policy Allow/Ask/Deny | 允许尝试/持久化等待人类/程序拒绝 | [第13章](policy.md) |
| Sandbox | OS层隔离；当前未实现，workspace只是文件能力范围与bash cwd | [第13章](policy.md) |
| Process group / Process tree | 信号可选集合 / 父子关系；脱组后代不受原组取消保证 | [第11章](06-process-cancellation.md) |
| Cancel / Timeout | 外部控制 / 时间预算；process timeout可以是正常ToolResult | [第11章](06-process-cancellation.md) |
| Durable queue / Steer | 输入事实进入history和FIFO；审批暂停可影响当前Turn，执行中输入延迟确认 | [第10章](session-actor.md) |
| Scheduler | 多SessionHandle门面，不复制历史，不自动持久化task绑定 | [第16章](11-scheduler.md) |
| Safe projection | v2只暴露有限字段，但input_summary仍可有命令/路径片段，不是通用脱敏 | [第14章](09-cli-and-protocol.md) |
| Execution Success / Task Correctness | 合法终态 / 独立验证满足任务；exit0与固定成功文本不足证明正确 | [第17章](config-tests-benchmarks.md) |

## 按场景理解 Rust

| 机制 | 先怎样读 | 本项目中的作用与限制 |
|---|---|---|
| trait | 某类组件必须提供哪些能力 | ModelProvider/Executor/Tool接口；不自动保证业务契约 |
| 泛型P/E/S | 装配时确定的具体类型 | AgentLoop静态使用组件；ConfiguredProvider枚举统一运行时选择 |
| dyn Trait | 运行时不同实现共用接口 | Registry存Arc dyn Tool；ToolContext借用dyn Executor |
| Arc | 多个所有者共享同一对象 | clone不是复制整份Executor；不自动允许无锁修改内部值 |
| Mutex | 进入共享数据的短临界区或异步锁 | Mock脚本短锁、store追加锁、stdout writer锁；作用范围各不同 |
| async/await | 一个可暂停等待的函数/future | 等HTTP/IO时让出调度；await不会自动生成线程或解除&mut借用 |
| tokio::spawn | 独立调度拥有数据的task | Provider/Tool/readers；task不持有&mut SessionState |
| tokio::select! | 轮询多个等待条件，选一个ready分支 | token/deadline/task/channel；biased指定优先次序，非所有使用都有biased |
| pin! | future位置保持稳定供重复poll | Actor反复select同一loop future，仍保有可变借用 |
| Result/问号 | Ok成功路径，Err提前交给上层 | Tool Err通常TurnFailed；ProcessResult exit7仍可Ok |
| Option | 明确存在或缺失 | active_turn=None与status不同；continuation可None |
| mpsc | 多发件人/单收件人长期通道 | SessionCommand、ProviderObserver；有界channel不代表所有后续Vec有界 |
| oneshot | 一次请求的一次回复 | start ack、wait outcome、attempt持久化ack |
| CancellationToken | 多任务可观察的协作取消信号 | clone同信号；child随父取消；cancel本身不写事件也不撤销副作用 |
| RAII / Drop | 值释放时尽力收尾资源 | permit/锁/临时文件/进程组guard；Harness被SIGKILL时不保证Drop运行 |
| spawn_blocking | 在线程池运行同步阻塞工作 | 文件锁/安全open syscalls，避免阻塞Tokio worker |
| &mut / 所有权 | 同时只允许一个可变借用 | Session直接调用限制并发；Actor只能等loop future返回再追加新输入 |

遇到语法先问“谁拥有值、谁能改、谁等待、失败交给谁”，再看其具体泛型形式；不要把语言工具误认成系统保证。

返回[教材首页](README.md) · [验证记录](validation.md)。
