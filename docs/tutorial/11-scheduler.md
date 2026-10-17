# 第 11 章：Scheduler、durable 队列和全局并发

这一章解释当前项目最后一组运行时语义：

- 一个 turn 运行时，新的用户输入怎样排队；
- 排队输入怎样在崩溃后保留；
- 多个 session 如何共享进程并发上限；
- Scheduler 为什么只做组合，不再复制 runtime 状态。

## 先看单 session 的队列

打开 src/runtime/session_actor.rs。actor 里有三个与队列相关的东西：

~~~text
queue
  └─ 已接受但尚未开始的 (TurnId, input)

deferred_waiters
  └─ 等待排队 turn 终态的回复通道

outcomes
  └─ 最近结束的 turn 结果，保留有限数量
~~~

当 active_turn 不存在时，StartTurn 直接开始；当 active_turn 存在时，actor 检查 max_queued_inputs，然后把输入 durable 地记录，再把它放入 queue。

输入被确认的时刻很重要：

~~~text
超出上限
  → QueueLimitExceeded
  → 没有 user.input.recorded

未超限
  → append user.input.recorded 成功
  → 回复 queued = true
  → 等前一个 turn 结束后自动开始
~~~

因此“客户端收到 queued = true”意味着输入已经成为事件事实。它不是一个只存在内存里的愿望。

## 两种 steer 时序

审批暂停时，turn future 已经让出了一部分控制权，actor 可以马上持久化新输入：

~~~text
turn A 等待审批
  → 输入 B 被记录
  → A 恢复采样时，history 已经看得到 B
  → A 终止
  → B 作为新的 turn 开始
~~~

模型采样或工具执行期间，AgentLoop future 暂时持有可变借用。actor 会先收下 StartTurn，等 future 结束后再 append：

~~~text
turn A 正在执行
  → 收到输入 B
  → 暂存 reply 和 input
  → A 终态，future 释放
  → append B
  → 回复 queued = true
  → B 开始
~~~

这两个场景都叫排队，但 B 对 A 的可见时间不同。tests/actor_queue_edges.rs 把这个差异固定成了测试。

## FIFO 和 WaitTurn

queue 是 FIFO。前一个 turn 结束后，drain_queued_turns 会：

1. 取队首；
2. begin_queued_turn；
3. 运行完整 turn；
4. 把结果写入 outcomes；
5. 唤醒对应的 deferred_waiters；
6. 继续处理下一条。

WaitTurn 如果收到一个还没开始的 turn_id，会把 reply 放入 deferred_waiters。它不会返回“unknown turn”，也不会忙等。

outcomes 只保留最近的一小段结果，防止内存无限增长。旧结果仍可以从 event log 和恢复工具中获得。

## 队列取消为什么没有默认支持

排队输入还没有开始，没有一个正在执行的副作用可取消。要取消它，需要记录一个“丢弃了哪条输入、为什么丢弃”的新事实，并让 reducer、恢复和协议都理解它。

当前项目选择的语义更简单：排队输入不单独取消；要么等它开始后取消，要么通过 abandon-turn 处理整个未完成工作。清晰的不支持比半成品的取消更容易恢复。

## Scheduler 的责任边界

打开 src/scheduler/mod.rs 和 src/scheduler/task.rs。Scheduler 组合多 session：

~~~text
Scheduler
  ├─ register_session
  ├─ submit
  ├─ wait
  ├─ cancel
  ├─ respond_approval
  ├─ run_state
  └─ shutdown
~~~

它不拥有 history、active_turn 或 event seq。真正的会话状态在每个 actor；Scheduler 只是把任务 ID 和对应的 SessionHandle 连接起来。

SchedulerTask 用 session id 和 turn id 标识一个工作项。它不是第二份状态机。

## 全局进程并发放在 executor

如果并发上限放在某个 turn 里，两个 session 各自允许 1 个进程时，全局仍然可能同时跑很多进程。

当前 LocalExecutor 可以共享 Semaphore：

~~~text
Session A ─┐
Session B ─┼─▶ shared LocalExecutor ─▶ max_concurrent_processes
Session C ─┘
~~~

run_process 先获取 OwnedSemaphorePermit，再启动进程；执行结束或出错时 permit 通过 RAII 释放。等待槽位本身也有预算，超时返回 ProcessError::ConcurrencyLimit，而不是无限排队。

这层限制属于 executor，因为它保护的是操作系统进程资源，而不是某个会话的策略决定。

## 生命周期投影

src/runtime/state.rs 的 run_state 会把 reducer state 投影成：

~~~text
Idle
RunningTurn
WaitingApproval
RunningTool
~~~

这个状态适合 UI 和 scheduler 查询。它是派生值，不需要单独写一条 lifecycle event。

审批状态优先于 RunningTool，因为有些批量工具调用会让一个执行在运行、另一个调用等待审批。显示“等待审批”能让客户端知道下一步需要谁做决定。

## 读测试

~~~bash
cargo test --test scheduler
cargo test --test actor_queue_edges
cargo test --test agent_loop_limits
~~~

先读队列满的测试，再读 FIFO 和 steer 测试，最后读 shared executor 的并发测试。这样可以看出：

- 上限检查发生在 append 前；
- ack 发生在 durable 事实之后；
- 共享 executor 才能限制跨 session 的真实进程；
- Scheduler 不需要自己维护一套状态。

## 思考题

1. 为什么 queued = true 必须晚于 user.input.recorded 的持久化？
2. 审批暂停时输入可以 steer 当前 turn，工具执行时输入却延迟到下一个 turn，这个差异来自什么借用和状态约束？
3. 如果 Scheduler 自己保存一份 history，会产生什么不同步场景？
4. 为什么并发上限放在 policy 会让“等待资源”和“禁止执行”混在一起？
5. 排队输入如果需要优先级，你需要追加哪些事实才能在恢复时重建顺序？
6. 一个 session 关闭时，queue 中的输入和 deferred_waiters 应该分别得到什么结果？
7. 为什么 semaphore permit 必须使用 RAII，而不能手动记得 release？

下一章：[尾声：把这份 Harness 读成一个系统](12-next-steps.md)。

