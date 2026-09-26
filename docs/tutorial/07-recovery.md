# 第 7 章：崩溃恢复——系统怎样面对“不知道”

前几章已经有 durable event log，也能取消进程。现在考虑一个无法回避的时刻：

~~~text
tool.started
进程崩溃
没有 tool.completed
~~~

本地日志只能证明工具开始了，不能证明外部世界最后发生了什么。当前项目没有假装自己拥有 exactly-once，而是把这种窗口明确表示为 OutcomeUnknown，并交给恢复流程分类。

## 恢复的三个动作

打开 src/durable/recovery.rs。恢复流程只做三件事：

1. 读取并重放已有事件；
2. 生成 RecoveryFinding；
3. 在显式 recover 命令中追加恢复事实和 action required。

它不会执行 read、edit、bash，也不会自动重新发送 provider 请求。

这条边界很重要。恢复代码运行时，外部世界可能已经改变；它只能解释日志，不能凭空确认副作用是否发生。

## RecoveryFinding 的含义

当前实现可能报告：

| Finding | 日志说明 |
|---|---|
| ActiveTurn | turn 没有到达终态 |
| PendingModelResponse | 模型请求可能在进程停止时仍未返回 |
| PendingApproval | 工具等待用户审批 |
| PendingTool | 工具请求已记录，但还没有开始 |
| UnknownTool | 工具可能已经产生副作用，但结果没有 durable |

PendingTool 和 UnknownTool 不同：

~~~text
tool.requested
  └─ PendingTool：可能尚未启动

tool.requested → tool.started
  └─ UnknownTool：已经进入执行窗口
~~~

恢复系统会为工具选择保守策略：

- read → RetryRead；
- edit → InspectEditHash；
- 其他工具，例如 bash → RequireUserDecision。

RecoveryPolicy 描述下一步该怎样处理，不代表系统已经替你处理了。

## inspect 和 recover 的区别

CLI 的 inspect 只读：

~~~text
读日志 → replay → classify_state → 打印 RecoveryReport
~~~

recover 会追加事实：

~~~text
recovery.started
tool.outcome_unknown（如果有已经开始的工具）
recovery.action_required
recovery.completed
~~~

recover 本身要幂等。重复运行时，不能每次都追加一套新的 recovery 事件。当前代码会检查已有 recovery batch 和 completed 事实。

abandon-turn 是人的显式决定。它追加 turn.failed，并让会话可以继续接受新的输入。恢复系统负责发现问题，人负责决定是否放弃。

## 先手工想象三种日志尾部

~~~text
A:
tool.requested
<EOF>
→ PendingTool

B:
tool.approval.requested
<EOF>
→ PendingApproval

C:
tool.started
<EOF>
→ UnknownTool
~~~

读 src/durable/recovery.rs 的 classify_state，观察它是如何从 TurnState.executions、approvals 和 tool_names 推导这些 finding 的。分类本身是纯函数，不读文件，也不调用 executor。

## checkpoint 是缓存，不是事实

打开 src/durable/checkpoint.rs。Checkpoint 包含：

- schema_version；
- session_id；
- last_seq；
- reducer state；
- state_digest。

它的作用是跳过很长日志的前缀。恢复时仍然要验证：

~~~text
checkpoint 的 session_id 正确
checkpoint 的 last_seq 存在于日志前缀
checkpoint marker 与快照相符
state_digest 能重新计算出来
后续事件可以继续 reduce
~~~

任何校验失败，replay_with_checkpoint_fallback 都应该从 event log 全量重放。删除或损坏 checkpoint 不应改变最终状态，只会变慢。

创建 checkpoint 时，marker 由 session actor 追加，快照文件通过临时文件、flush、sync 和 rename 原子替换。这样事件序列不会被旁路 writer 插入，也不会留下一个已经领先日志的快照。

## restore 如何重建 AgentLoop

src/runtime/agent_loop_setup.rs 中有两条路径：

- restore：从头读取事件并 reduce；
- restore_with_checkpoint：优先使用 checkpoint，校验失败时回退全量 replay。

两条路径最后都必须确认 state.last_seq 与 store.last_seq 一致。恢复成功后的 AgentLoop 拥有新的内存状态，但事实仍然来自原来的日志。

注意预算的语义：重启后的 turn 会获得新的运行时预算，原来的 turn_steps 和 tool_time_used 不直接从进程内计时器恢复。durable 事实恢复了状态，实时计时器需要重新开始。

## 读测试

~~~bash
cargo test --test recovery_classify
cargo test --test recovery_cli
cargo test --test checkpoint
cargo test repair_partial_tail
~~~

重点关注：

- 五种 finding 是如何构造的；
- recover 第二次运行为什么不重复追加；
- 损坏 checkpoint 为什么回退；
- 半行修复为什么只允许操作最后一行；
- inspect 输出为什么要脱敏工具输入。

## 思考题

1. 为什么恢复流程不能自动重试 read，即使 read 通常没有副作用？
2. tool.started 后没有结果时，为什么不能直接记录 tool.failed？
3. checkpoint 损坏时全量 replay 能恢复什么，不能恢复什么？
4. recovery.completed 为什么需要成为事实，而不能只在 CLI 返回成功？
5. 如果工具是“发送邮件”，policy_for_tool 应该返回什么？
6. abandon-turn 之后，为什么新的 user.input.recorded 可以把会话从 Failed 拉回 Idle？
7. 如果 checkpoint 写入失败但 marker 已追加，下一次创建 checkpoint 应该怎样处理？

下一章：[真实 provider——Responses 与 Chat Completions](08-openai-provider.md)。

