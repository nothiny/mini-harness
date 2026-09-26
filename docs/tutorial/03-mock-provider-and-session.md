# 第 3 章：MockProvider、Session 和 actor

这一章把前两章的类型和事件放进一次真实 turn。重点不是模型回答得多聪明，而是看 runtime 怎样在没有网络的情况下验证自己的语义。

当前项目有两种使用 Session 的方式：

- 直接使用 Session：适合测试、CLI 和库调用；
- 使用 SessionHandle：背后启动 session actor，适合协议、TUI 和需要中途取消的调用方。

## 先看 Provider trait

打开 src/model/provider.rs。最重要的类型是：

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
  └─ complete(request, cancel) -> ModelResponse
~~~

Provider 只负责把请求送给模型并把响应翻译成统一类型。它不应该：

- 直接修改 SessionState；
- 直接追加事件；
- 直接执行工具；
- 决定一个工具是否有权限执行。

这些行为属于 runtime、policy 和 executor。

ModelRequest 是拥有所有权的值。Provider 收到它之后，可以把它交给 HTTP client、mock 脚本或别的实现，但不会拿到 AgentLoop 的可变引用。这样 provider 可以替换，主循环不用跟着变。

## MockProvider 为什么重要

打开 src/model/mock.rs。MockProvider 通常包含两部分：

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

## 直接使用 Session 的路径

打开 src/runtime/session.rs 中的 Session 和构造函数。直接使用 Session 的调用大致是：

~~~text
Session::new_with_policy(...)
  └─ 内部创建 AgentLoop

Session::start_turn(input)
  └─ AgentLoop::run_turn(input)
      └─ 返回 (TurnId, final_text)
~~~

这种方式简单，适合单线程式的库调用。它的限制也很明确：start_turn 正在运行时，调用方没有另一个命令入口可以发送 cancel。

这就是为什么 CLI 中的简单 run 可以使用 Session，而 TUI、JSONL server 和跨进程操作通常使用 actor handle。

## actor 的路径

SessionHandle 背后是一个 tokio task。调用方不直接碰 AgentLoop，而是发 SessionCommand：

~~~text
调用方
  │
  ├─ mpsc::Sender<SessionCommand>
  │
  ▼
session_actor::run_actor
  │
  ├─ 唯一持有 AgentLoop
  ├─ 唯一追加事件
  └─ oneshot::Sender 返回本次命令的结果
~~~

当前 SessionCommand 包含 StartTurn、CancelTurn、ApproveTool、WaitTurn、Checkpoint、QueryState 和 Shutdown。

mpsc 传命令，oneshot 回结果。mpsc 适合连续接收请求，oneshot 适合“一条命令对应一个回复”。

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

错误路径同样要记录事实。例如 provider 失败时，不能只把 Err 返回给调用方；runtime 还要追加 turn.failed。否则日志会停在 Running，恢复系统无法知道这是一个已经结束的失败 turn，还是进程在执行中崩溃。

## actor 为什么不是 Arc<Mutex<Session>>

共享一个 Mutex 也能让代码运行，但它会把几个问题混在一起：

- 谁可以在 await 期间持有锁；
- 状态迁移和事件追加是否同时发生；
- cancel 和 start 谁先修改状态；
- 一个命令失败后，锁内状态是否已经改变。

actor 的规则更直接：只有 actor 能修改 AgentLoop，其他组件只能发命令。逻辑上的原子性由命令顺序和 append 事务保证。

当前 actor 在活跃 turn 期间会 select 命令通道：

- CancelTurn 会触发 turn token；
- WaitTurn 会保存 waiter；
- StartTurn 会暂存，等 AgentLoop future 释放可变借用后再 durable 地排队；
- Checkpoint 会延迟到安全点；
- Shutdown 会取消并等待清理。

“收到命令”和“马上修改 state”不是同一个时刻。这个区别是读 session_actor.rs 时最容易漏掉的地方。

## 读测试的方式

运行：

~~~bash
cargo test mock_script_exhaustion_is_a_provider_error
cargo test provider_error_records_turn_failed
cargo test wait_turn_observes_provider_failure_after_start_ack
cargo test session_actor
~~~

读 actor 测试时，画两条时间线：

~~~text
StartTurn 的 ack 什么时候返回
WaitTurn 的结果什么时候返回
~~~

这两个时间点不一定相同。StartTurn 说明请求已经被接受，WaitTurn 说明 turn 已经有最终结果。

## 这一章读完后

你应该能解释：

- MockProvider 为什么不只是测试替身；
- ModelRequest 为什么必须是拥有所有权的快照；
- Session 和 SessionHandle 的使用场景差异；
- actor 如何保证只有一个写者；
- provider 错误为什么也要留下 turn.failed。

## 思考题

1. 如果 Provider trait 接收的是 &mut SessionState，会出现哪两种边界混乱？
2. 为什么 StartTurn 和 WaitTurn 要分开？把它们合成一个阻塞调用会让哪个客户端难以实现？
3. actor 在执行模型请求时仍然需要接收 CancelTurn。取消信号经过哪些对象到达 provider？
4. 如果 MockProvider 的 script 耗尽后 panic，为什么会让恢复语义变差？
5. 直接使用 Session 的测试很方便。你会在什么条件下把它改成 SessionHandle？

下一章：[Tool、Policy 和 Executor——一次 read 怎样到达文件系统](04-first-tool.md)。

