# 第 5 章：AgentLoop——一轮 turn 怎样反复采样

前几章分别讲了事件、provider 和工具。这一章把它们放在同一个控制循环里。

读完以后，你应该能从 src/runtime/agent_loop.rs 解释：用户输入怎样变成第一次模型请求，工具结果怎样进入下一次请求，一轮 turn 在什么情况下完成或失败。

## 先找真正的入口

当前 AgentLoop 的主要入口分布在两个文件：

- src/runtime/agent_loop_setup.rs：构造新 loop、从日志恢复 loop；
- src/runtime/agent_loop.rs：append、开始 turn、继续 turn、取消和限制。

先看方法之间的关系：

~~~text
run_turn(input)
  └─ run_turn_with_cancel(input, token)
       └─ run_turn_with_reason(...)
            ├─ begin_turn(...)
            └─ continue_turn(...)
                 └─ continue_turn_with_reason(...)
~~~

begin_turn 负责把用户输入和 turn.started 变成事实。continue_turn_with_reason 才是反复请求 provider、处理工具调用和判断终态的循环。

## 主循环的每一轮

把实际代码压缩成伪代码：

~~~text
while turn 还没有终态:
    检查 cancel、step、turn time、tool time
    从 SessionState 构造 ContextSnapshot
    转成 ModelRequest
    调 provider，得到 ModelCompletion

    如果 continuation 改变:
        append provider.continuation.updated

    如果响应是 Text:
        append model.response.recorded
        append turn.completed
        返回最终文本

    如果响应是 ToolCall 或 ToolCalls:
        检查是否为空、是否重复、是否超过 batch 上限
        append model.response.recorded
        对每个 call:
            append tool.requested
            通过 policy
            Allow → append tool.started → 执行 → append tool.completed/failed
            Ask → append tool.approval.requested，暂停
            Deny → append tool.policy_denied，结束或继续
        回到下一轮采样
~~~

这段循环没有直接修改 history。所有改变都经过 append，再由 reducer 更新 state。

## ModelResponse 为什么也会记录工具调用

一个模型响应可以是文本，也可以是工具调用。当前实现把工具调用转换成一个可观察的 model.response.recorded，内容类似：

~~~text
tool call: read
tool calls: read, bash
~~~

它不是工具结果，也不替代 tool.requested。它表示“模型这次说了什么”；tool.requested 表示“runtime 接受了哪个调用并开始处理状态机”。

工具调用执行后，ToolCompleted 才会把 ToolResult 加入 history，供下一次 ModelRequest 使用。

## ContextSnapshot 是模型看到的窗口

打开 src/runtime/context.rs。ContextSnapshot 是拥有所有权的、不可变的模型输入：

~~~text
SessionState
  ├─ history
  ├─ provider_continuation
  └─ tools
       │
       ▼
ContextSnapshot
  ├─ 有限 history
  ├─ 有限的每项文本
  ├─ tools
  └─ continuation
       │
       ▼
ModelRequest
~~~

它有两个重要限制：

- 单条 history item 默认最多 16 KiB；
- provider 看到的 history 总量默认最多 512 KiB。

截断只发生在 ContextSnapshot，不会删除 durable history。日志保留完整事实，模型请求只携带当前预算允许的部分。

如果有 provider continuation，history_start 会根据 history_cursor 选择需要发送的历史窗口。续传 provider 和完整回放 provider 都通过同一个 ModelRequest 接口工作。

## append 是主循环的安全边界

AgentLoop::append 的顺序必须牢记：

~~~text
1. expected_seq = state.last_seq + 1
2. 确认 store 没被其他进程追加
3. 检查事件序列化后的大小
4. 在 next_state 副本上调用 reduce
5. store.append(event)
6. 检查 store 返回 seq
7. 提交 self.state = next_state
~~~

如果先修改 self.state，再写日志，进程在中间崩溃，内存曾经看到过一个事实，日志却没有它。恢复时就会出现两个世界。

如果 provider 响应产生空工具批次、重复 call_id、超过 max_batch_tool_calls 或超过 step 上限，runtime 会先记录 turn.failed，再返回错误。错误路径也必须是事件路径。

## 取消、超时和审批

取消和超时有不同的事件：

~~~text
用户取消
  └─ turn.cancel_requested
  └─ turn.cancelled

时间预算耗尽
  └─ turn.timed_out
~~~

工具的取消还要等 executor 收尾。审批则是另一种暂停：

~~~text
tool.requested
  → tool.approval.requested
  → turn 保持 active
  → 用户批准
  → tool.approval.responded
  → tool.started
  → 工具完成
  → 回到采样
~~~

审批不是 provider 错误。它是一个已经持久化的控制状态，所以 UI 或协议客户端重启后仍然可以看到“正在等待审批”。

## 建议的阅读实验

运行：

~~~bash
cargo test --test integration_agent_loop
cargo test agent_loop_passes_system_instructions_through_snapshot
cargo test --test agent_loop_limits
~~~

在 integration_agent_loop.rs 中找一个脚本为 ToolCall、Text 的测试，画出两次 ModelRequest：

~~~text
request 1: user history + tool specs
request 2: user history + tool result + tool specs
~~~

再找一个 max_steps 或空批测试，看它在事件日志里记录哪个终态。

## 思考题

1. 为什么工具结果必须进入下一次 ModelRequest，而不能只存在 ToolCompleted 事件里？
2. ContextSnapshot 截断 history 后，为什么 durable log 仍然要保留完整文本？
3. 如果模型一次返回 100 个工具调用，最早应该在哪一层限制？
4. append 的副本 reduce 能防止哪些问题，不能防止哪些外部副作用？
5. 为什么 turn.cancel_requested 和 turn.cancelled 要分成两个事实？
6. 审批暂停时，Session actor 为什么可以处理新输入；工具执行期间又为什么会延迟处理？

下一章：[进程、取消和原子编辑](06-process-cancellation.md)。

