# 第 11 章：并发边界——单写者、输入槽与进程并发上限

这一章回答最后一组运行时问题：

- 同一个 session，谁有权修改状态、追加事件？
- turn 运行期间来了新输入，会发生什么？
- 多个 session 同时跑 `bash`，谁来限制总进程数？

旧版本曾用 session actor、durable 输入队列和 Scheduler 回答这些问题。简化版把它们全部删掉（见 `SIMPLIFICATION_GUIDE.md`），换成更朴素的机制。这一章讲清楚：删掉之后，靠什么仍然保证正确。

## 单写者现在由借用检查器保证

打开 `src/runtime/session.rs`。`Session` 是 `AgentLoop` 之上的同步门面：

~~~text
pub struct Session<P, E, S> {
    loop_: AgentLoop<P, E, S>,
}
~~~

`start_turn(&mut self, ...)` 拿的是独占借用。turn 结束前，调用方无法对同一个 `Session` 调用任何其他方法——不是靠运行时约定，是靠编译器。turn 未结束时再开一个 turn，得到的是明确的 `HarnessError::TurnAlreadyActive`。

对比一下两种实现单写者的方式：

~~~text
actor 版本（已删除）
  mpsc 命令通道 + tokio task + oneshot 回复
  正确性依赖：所有写路径都经过通道

同步版本（当前）
  &mut self 独占借用
  正确性依赖：借用检查器，零运行时成本
~~~

删掉 actor 损失了什么？turn 运行**期间**没有第二个入口可以插入命令。这个损失用三条路补回来：

1. **审批暂停**：`ask` 工具让 `start_turn` 以 `ApprovalPending` 错误返回，future 已结束，借用已释放——这时可以调 `approve_tool` 或 `cancel_turn`；
2. **墙钟超时**：`start_turn_with_timeout` 给 turn 一个明确期限；
3. **跨进程取消**：`mini-harness cancel` 从另一个进程写终态事件（第 7 章）。

也就是说：暂停是**返回错误**，不是 future 挂起等用户。这让"暂停点"成为一个普通的函数返回值，而不是一个隐藏的并发状态。

## 输入语义：一个槽，不做队列

打开 `src/runtime/state.rs`：

~~~text
pub pending_input: Option<UserInput>
~~~

不是 `VecDeque`，是一个 `Option`。reducer 的规则（`src/durable/reducer.rs`）：

- `user.input.recorded` 只有在**没有活跃 turn 且槽为空**时才被接受；
- `turn.started` 消费这个槽；
- turn 运行期间（或槽未消费时）新输入直接被拒绝——runtime 层报 `TurnAlreadyActive`，reducer 层拒绝非法事件。

这是和旧版 durable 队列最锋利的对比：旧版把排队输入也变成事实（崩溃后队列还在），代价是 actor、FIFO 语义和一堆边界情况；简化版承认"学习项目不需要排队"，把拒绝变成一个明确错误。**清晰的不支持，比半成品的支持更容易理解。**

## 需要真正的并发？在外层组合

协议服务器就是现成的范例（`mini-harness-server/src/jsonl.rs`）：

~~~text
turn.start
  → Session::begin_turn     记录输入、拿到 turn_id，立即返回
  → tokio::spawn(drive_turn)   后台任务驱动 continue_turn
  → 回复客户端 { turn_id }

turn.wait
  → 查 outcomes 表 / 等待通知
  → { text, completed } 或 turn_pending（超时）

turn.cancel / approval.respond
  → 对同一个 session 槽加锁后调用 cancel_turn / approve_tool
~~~

注意关键设计：Session 本身不知道服务器的存在。服务器用 `begin_turn` + `continue_turn` 的拆分接口把"接受请求"和"驱动执行"解耦，再用 `tokio::spawn` 和锁在外面组合出并发。**并发策略是调用方的事，核心保持单线程可推理。**

## 进程并发上限：留在 executor 层

有一个并发控制没有被删——它保护的是操作系统，不是会话语义。打开 `src/executor/local.rs`：

~~~text
LocalExecutor::with_process_limits(max_concurrent)
  → tokio::sync::Semaphore
  → run_process 先拿 OwnedSemaphorePermit 再 spawn
  → permit 由 RAII 释放（执行结束、出错、取消都覆盖）
~~~

配置来自 `[execution] max_concurrent_processes`（默认 8）。超限时 `run_process` 返回结构化的 `ConcurrencyLimit` 错误，而不是无限排队。

为什么这层不删？因为不管上层怎么简化，两个 session 同时各跑 10 个编译进程，机器照样会被打爆。而且它必须放在 executor 而不是某个 turn 里——单 turn 的限制管不住跨 session 的总量。semaphore 在 `LocalExecutor` 内部，天然被所有共享同一个 executor 的 session 继承。

## 一次完整的边界巡检

~~~text
同一 session 的状态写入
  → 借用检查器（&mut Session）

turn 期间的新输入
  → TurnAlreadyActive，明确拒绝

跨进程对同一日志的写入
  → append 前检查 store.last_seq，out-of-sync 即停止

跨 session 的进程总量
  → executor 层 semaphore
~~~

四条边界，四种不同的机制：编译期、durable 状态机、写前校验、资源信号量。读代码时先分清"这条边界防的是谁"，再问"为什么是这一层的机制"。

## 读测试

~~~bash
cargo test --test cross_process_cancel        # 跨进程写边界
cargo test --test process_executor            # 进程并发与输出上限
cargo test --test integration_agent_loop      # TurnAlreadyActive 与审批暂停
cargo test -p mini-harness-server --test stdio_protocol   # 外层组合并发
~~~

## 思考题

1. 用 `Arc<Mutex<Session>>` 也能实现单写者。它和 actor、和 `&mut` 各差在哪？哪种错误会从编译期推迟到运行期？
2. `pending_input` 为什么必须是 reducer 状态的一部分，而不是服务器内存里的一个字段？
3. 审批暂停用"返回 ApprovalPending 错误"实现。如果改成"future 挂起等用户输入"，崩溃恢复语义会变差在哪里？
4. 把进程并发上限放进 policy 层会有什么问题？（提示：等待资源和禁止执行是两件事）
5. 服务器为每个 turn spawn 一个任务。如果客户端断开连接，这个任务应该怎么办？断连和用户取消应该记录成同一个事实吗？
6. 如果要恢复旧版的 durable 输入队列，最少需要新增哪些事件和 reducer 规则？

下一章：[尾声：把这份 Harness 读成一个系统](12-next-steps.md)。
