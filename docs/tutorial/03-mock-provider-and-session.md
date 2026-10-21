# 第 3 章：MockProvider 与 Session——不联网验证主循环

这一章把前两章的类型和事件放进一次真实 turn。重点不是模型回答得多聪明，而是看 runtime 怎样在没有网络的情况下验证自己的语义。

## 先看 Provider trait

打开 `src/model/provider.rs`。最重要的类型是：

~~~text
ModelRequest
  ├─ model
  ├─ system_instructions
  ├─ history
  ├─ tools
  └─ continuation

ModelResponse
  ├─ Text
  ├─ ToolCall
  ├─ ToolCalls
  └─ TextWithToolCalls

ModelProvider
  └─ complete(request, cancel, observer) -> ModelCompletion
~~~

Provider 只负责把请求送给模型并把响应翻译成统一类型。它不应该：

- 直接修改 SessionState；
- 直接追加事件；
- 直接执行工具；
- 决定一个工具是否有权限执行。

这些行为属于 runtime、policy 和 executor。

ModelRequest 是拥有所有权的值。Provider 收到它之后，可以把它交给 HTTP client、mock 脚本或别的实现，但不会拿到 AgentLoop 的可变引用。这样 provider 可以替换，主循环不用跟着变。

## MockProvider 为什么重要

打开 `src/model/mock.rs`。MockProvider 通常包含两部分：

~~~text
script
  └─ 按顺序取出的 MockResponse

requests
  └─ 保存每次收到的 ModelRequest，供测试检查
~~~

例如 demo 的 read 脚本是：

~~~text
1. ToolCall(name = read, input = {"path":"README.md"})
2. Text("read complete")
~~~

它模拟模型先请求工具，再看到工具结果后给出最终文本。脚本耗尽时返回结构化 provider 错误，不应该 panic。

requests 让我们能观察上下文是否正确：

~~~text
第 1 次 request.history
  └─ 用户输入

工具完成之后的第 2 次 request.history
  ├─ 用户输入
  └─ HistoryItem::Tool { result: ... }
~~~

如果第二次请求没有工具结果，runtime 的闭环就没有完成。

## Session：AgentLoop 的同步门面

打开 `src/runtime/session.rs`。`Session` 只包着一个 `AgentLoop`，提供几组入口：

~~~text
new / new_with_policy            构造新会话
resume / resume_with_policy      重放日志恢复会话（上一章的 restore）
start_turn(input)                跑完一个 turn，返回 (turn_id, text)
start_turn_with_timeout          带墙钟期限
cancel_turn / approve_tool       审批暂停后的中途控制
begin_turn / continue_turn       拆分接口：先拿 id，再驱动执行
~~~

直接使用 Session 的调用方式：

~~~text
Session::new_with_policy(...)
  └─ 内部创建 AgentLoop

Session::start_turn(input)
  └─ AgentLoop::run_turn(input)
      └─ 返回 (TurnId, final_text)
~~~

这种方式适合 CLI 的 `run`、测试和示例代码：一条主线跑到底，没有并发。

## 单写者靠什么保证

`start_turn(&mut self, ...)` 拿的是独占借用。turn 结束前，调用方无法对同一个 `Session` 调用任何其他方法——不是靠运行时约定，是靠借用检查器。turn 未结束时再开 turn，得到 `HarnessError::TurnAlreadyActive`。

那 turn 运行**期间**需要干预怎么办（取消、审批）？三条路：

- **审批暂停**：`ask` 工具让 `start_turn` 以 `ApprovalPending` 错误返回。future 已经结束，借用已释放，这时可以调 `approve_tool`（批准并继续）或 `cancel_turn`（放弃）；
- **墙钟超时**：`start_turn_with_timeout`；
- **跨进程取消**：`mini-harness cancel` 写终态事件，运行进程在下一次 append 时发现日志变化而停止。

关键理解：暂停是**返回错误**，不是 future 挂起等输入。`ApprovalPending` 是一个普通的返回值，turn 的状态机停在"等待审批"这个持久化状态上，进程甚至可以崩溃——重启重放后状态不变。

## begin_turn / continue_turn 为什么存在

协议服务器（第 9 章）需要在 turn 开始的瞬间就回复客户端一个 turn_id，而不是等 turn 跑完。所以 Session 提供拆分接口：

~~~text
begin_turn(input) -> TurnId       记录 user.input.recorded + turn.started，立即返回
continue_turn(id, cancel)         驱动采样、工具、终态，直到 turn 结束
~~~

服务器先 `begin_turn` 拿到 id 回复客户端，再 `tokio::spawn` 一个任务驱动 `continue_turn`。并发是调用方组合出来的，Session 核心保持单线程可推理——这是简化版删掉 session actor 后的替代设计（详见第 11 章）。

## 一个文本 turn 怎样走完

对于 MockProvider 返回 Text 的情况，主路径可以简化成：

~~~text
Session::start_turn
  → begin_turn
      → user.input.recorded
      → turn.started
  → ContextSnapshot::from_state
  → ModelProvider::complete
  → provider.continuation.updated（有变化时）
  → model.response.recorded
  → turn.completed
  → 返回文本
~~~

错误路径同样要记录事实。例如 provider 失败时，不能只把 Err 返回给调用方；runtime 还要追加 `turn.failed`。否则日志会停在 Running，恢复系统无法知道这是一个已经结束的失败 turn，还是进程在执行中崩溃。

## 读测试的方式

运行：

~~~bash
cargo test mock_script_exhaustion_is_a_provider_error
cargo test provider_error_records_turn_failed
cargo test --test integration_agent_loop
cargo run --example 02_agent_loop
~~~

`examples/02_agent_loop.rs` 用三十行代码把"构造组件 → start_turn → 打印事件"走了一遍，是本章最好的浓缩版。读测试时，画两条时间线：

~~~text
start_turn 的返回什么时候发生
turn 的终止事件什么时候落盘
~~~

文本 turn 两者几乎同时；带审批的 turn 中间隔着一个用户决定。这个间隔就是"暂停 = 返回错误"设计的直观体现。

## 这一章读完后

你应该能解释：

- MockProvider 为什么不只是测试替身；
- ModelRequest 为什么必须是拥有所有权的快照；
- 单写者怎样由借用检查器而不是 actor 保证；
- 审批暂停为什么是返回错误而不是挂起等待；
- begin_turn / continue_turn 的拆分为谁服务。

## 思考题

1. 如果 Provider trait 接收的是 `&mut SessionState`，会出现哪两种边界混乱？
2. 为什么不做一个 `wait_for_approval()` 让 future 挂起等用户？对比"返回错误 + 状态机暂停"的恢复语义。
3. 同一个 Session 被 `Arc<Mutex<>>` 包住后跨 task 共享，和 actor 模型差在哪？哪种问题会被推迟到运行期？
4. 如果 MockProvider 的 script 耗尽后 panic，为什么会让恢复语义变差？
5. `resume` 之后立刻 `start_turn`，如果崩溃前的 turn 还没终止，会发生什么？应该先做什么？

下一章：[Tool、Policy 和 Executor——一次 read 怎样到达文件系统](04-first-tool.md)。
