# 第 7 章：崩溃恢复——重放、结果未知与刻意不做的事

前几章已经有 durable event log。现在考虑一个无法回避的时刻：

~~~text
tool.requested
进程崩溃
没有 tool.completed
~~~

本地日志只能证明工具被请求了，不能证明外部世界最后发生了什么。本章讲三件事：崩溃后怎样恢复（答案是完整重放）、哪类结果必须承认"不知道"、以及简化版刻意删掉了哪些恢复机制。

## 恢复就是重放

打开 `src/runtime/agent_loop_setup.rs` 的 `AgentLoop::restore`。它只做四步：

~~~text
read_from(seq 1)     读整个日志
  → 逐条 reduce       任何非法转换都会让恢复失败
  → 校验 state.last_seq == store.last_seq()
  → from_state(...)   用重放出的状态构造新的 AgentLoop
~~~

宁可失败也不猜测历史。三种输入会直接报错：

- 空日志（没有 session.created）；
- 日志属于另一个 session；
- 恢复期间日志被别的进程追加了（`store changed while restoring`）。

没有 checkpoint，没有快照缓存——确定性 reducer 让"完整重放"就是正确的恢复算法。日志有多长，恢复就有多慢；这是简化版用性能换简单性的明确取舍（见 `SIMPLIFICATION_GUIDE.md`）。生产系统会加 checkpoint 加速，但 checkpoint 只是缓存，事实永远在日志里。

## 重放后 turn 还在运行，是什么意思

如果日志末尾是：

~~~text
session.created
user.input.recorded
turn.started
（没有终止事件）
~~~

重放出的状态里 `active_turn` 仍然存在。这不是 bug，是事实的正确读法：崩溃发生在 turn 中间，日志只能证明到这里。

调用方接手后有两个选择：

- `cancel_turn`：追加 `turn.cancelled`，显式放弃这个 turn；
- 对挂起的审批调用 `approve_tool`：如果崩溃前 turn 正停在 `ApprovalPending`。

## 预算不恢复，只有事实恢复

`from_state` 的注释值得读三遍：恢复后的 turn 拿到的是**新的**运行时预算——`turn_started_at` 是"现在"，`turn_steps` 和 `tool_time_used` 归零。

这是刻意的：进程停机时间不应该瞬间耗尽一个 turn 的墙钟预算。但也意味着：durable 事实（历史、执行状态、审批状态）跨崩溃存活，实时计时器不存活。恢复语义是"从事实继续"，不是"从崩溃点无缝续传"。

## 三类工具结果，只有一类承认不知道

一次工具调用可能落进三种事件：

~~~text
tool.completed { outcome: succeeded }   执行完成，结果确定
tool.completed { outcome: rejected }    执行前就被拒绝（未知工具、参数校验失败），无副作用
tool.completed { outcome: failed }      执行后失败，副作用可能已发生
tool.outcome_unknown                    执行被中断且结果不可知
~~~

`outcome_unknown` 出现在：取消时清理超时、工具超时被杀、工具任务 panic。它和 `failed` 的差异：

~~~text
Failed
  → 可以确定工具没有完成预期操作

OutcomeUnknown
  → 本地日志无法证明外部操作是否发生
~~~

注意 `rejected` 和 `failed` 的分工：参数写错是 rejected，turn 不终止，模型可以在下一步纠正；`bash` 命令执行到一半报错是 failed，副作用可能已经发生，turn 必须终止。区分标准不是"错误严不严重"，而是"副作用是否可能已经发生"。

## 半行损坏：修复是显式命令

JSONL 日志的最后一行可能写了一半（进程在 write 和 flush 之间死掉）。`JsonlEventStore` 的规则：

1. 读取时发现半行，报告损坏——不静默丢弃；
2. `repair_partial_tail` 是显式的修复命令：只允许截掉最后半行；
3. 中间损坏的行不能修，只能人工处理。

CLI 的 `events`、`inspect`、`resume`、`cancel` 打开日志时都会先调用 `repair_partial_tail`。为什么读操作也带修复？因为这些命令的语义是"给我一个可重放的日志"，而半行是崩溃残留，不是事实。

## 跨进程取消：宁可失败也不覆盖

`mini-harness cancel <session-id> <turn-id>` 由另一个进程执行：打开同一个日志，重放确认 turn 仍在运行，然后追加 `turn.cancel_requested` 和 `turn.cancelled`。

正在运行的进程怎么知道？看 `src/runtime/agent_loop.rs` 的 `append`：每次追加前先检查 `store.last_seq() == state.last_seq`。发现日志已经被别人写过，就以 out-of-sync 错误停止——绝不覆盖另一个写者的事实。

这留下一个现实窗口：被取消进程的工具可能已经产生副作用，但它无法把结果追加进日志。恢复工具（`inspect`）能把这个窗口显示出来；怎么处置，是人的决定。

## 时间旅行调试

确定性 reducer 还送了一个调试能力：`mini-harness inspect <log> --until <seq>` 只重放前 `seq` 条事件，打印那个历史时刻的状态。日志变成了一卷可以任意倒带的录像带——这就是事件溯源的调试体验，不需要任何额外机制。

动手试一遍：

~~~bash
cargo run -- run "hello" --event-log /tmp/demo.jsonl --workspace .
cargo run -- events /tmp/demo.jsonl              # 先看全部事件和 seq
cargo run -- inspect /tmp/demo.jsonl --until 3   # 重放到第 3 条：turn 刚启动
cargo run -- inspect /tmp/demo.jsonl             # 全量重放：turn 已完成
~~~

`cargo run --example 04_crash_recovery` 用几十行代码演示了完整的"模拟崩溃 → 重放 → 发现活跃 turn"闭环，值得逐行读。

## 读测试

~~~bash
cargo test --test cross_process_cancel
cargo test --test jsonl_cross_process
cargo test --test tool_input_recovery
cargo test --test reducer_turn_terminal
~~~

重点关注：跨进程 cancel 为什么不覆盖日志；半行修复为什么只允许操作最后一行；重放两次为什么得到完全相同的状态。

## 思考题

1. 为什么恢复流程不自动重试一个 `outcome_unknown` 的 read，即使 read 通常没有副作用？
2. `tool.requested` 之后没有结果时，为什么不能直接补一条 `tool.completed { outcome: failed }`？
3. 重放后 `active_turn` 仍存在。为什么 reducer 不在恢复时自动把它标记为失败？
4. 如果要加 checkpoint，它必须满足什么校验才能不破坏"日志是唯一事实"？
5. `inspect --until` 和全量 `inspect` 在什么场景下会给出不同的 `status`？
6. 恢复后 turn 拿到新的时间预算。这个决定在什么场景下是错的？你会怎么改？

下一章：[真实 provider——Responses 与 Chat Completions](08-openai-provider.md)。
