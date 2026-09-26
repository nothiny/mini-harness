# 第 2 章：EventStore 和 reducer——日志怎样变成状态

第 1 章介绍了事件的形状。这一章追问：事件写在哪里？重启后，系统怎样知道当前会话进行到哪一步？

当前项目把答案拆成两个模块：

- EventStore 负责保存和读取事件；
- reducer 负责把事件应用到 SessionState。

这两个模块分开，是整个恢复能力的基础。

## 先看三个真实文件

按这个顺序打开源码：

1. src/runtime/state.rs：SessionState、TurnState、HistoryItem；
2. src/durable/reducer.rs：reduce；
3. src/durable/store.rs、src/durable/memory.rs、src/durable/event_log.rs：EventStore 及实现。

不要一开始读 event_log.rs 的所有文件锁和缓存。先理解下面的纯数据流：

~~~text
事件日志
  └─ Event 1
  └─ Event 2
  └─ Event 3
       │
       ▼
reduce(state, event)
       │
       ▼
SessionState
  ├─ status
  ├─ active_turn
  ├─ history
  ├─ pending_inputs
  ├─ provider_continuation
  └─ last_seq
~~~

SessionState 是“从日志计算出来的当前视图”。它很重要，但它不是唯一事实。进程崩溃后，runtime 可以重新从事件构造它。

## SessionState 里最值得先读的字段

src/runtime/state.rs 中的 SessionState 有几个字段：

| 字段 | 含义 |
|---|---|
| session_id | 这份状态属于哪个会话 |
| status | Idle、Running、Failed 或 Closed |
| active_turn | 当前是否有正在处理的 turn |
| history | 给后续模型请求使用的对话历史 |
| pending_inputs | turn 运行时排队的用户输入 |
| provider_continuation | 下一次 provider 请求需要的续传信息 |
| last_seq | 已应用到状态的最后一条事件序号 |

TurnState 再记录某一轮内部发生的事情：模型最近的响应、工具调用 ID、每个工具的 ExecutionState、审批状态和最终文本。

注意“当前有几个工具正在运行”没有单独存成一个事实字段。它由 active_turn.executions 中的 Running 状态推导出来。

## reducer 既是更新器，也是验证器

打开 src/durable/reducer.rs 的 reduce。它一开始就做三类检查：

~~~text
1. event.session_id 必须等于 state.session_id
2. event.seq 必须等于 state.last_seq + 1
3. 空状态的第一条事件必须是 SessionCreated
~~~

然后它按 EventPayload 分支处理。例如：

- UserInputRecorded 把用户输入写进 history，并加入 pending_inputs；
- TurnStarted 从 pending_inputs 取出一条，创建 active_turn；
- ToolRequested 创建 Pending execution；
- ToolStarted 把 Pending 变成 Running；
- ToolCompleted 把 Running 变成 Completed，并把工具结果放进 history；
- TurnCompleted 关闭 active_turn；
- TurnFailed、TurnCancelled 和 TurnTimedOut 结束当前 turn。

这些分支同时验证状态迁移是否合法。因此以下事件顺序会被拒绝：

~~~text
ToolCompleted 出现在 ToolStarted 之前
同一 call_id 被完成两次
TurnCompleted 时仍有 Running 工具
另一个 session 的事件混进来
seq 从 41 跳到 43
~~~

严格拒绝的好处是：恢复时不需要猜日志作者想表达什么。一个能被打开的日志就是一个通过状态机检查的日志。

## EventStore 的两个实现

EventStore trait 只有三个核心操作：

~~~text
append(event)      追加事实，并返回最终保存的事件
read_from(seq)     从某个序号开始读取
last_seq()         查询最后一个序号
~~~

InMemoryEventStore 把事件放在 Vec 中，适合测试和 demo。JsonlEventStore 把每个事件写成一行 JSON，适合持久化会话。

两者对调用方提供同一套语义，但职责不完全相同：

- AgentLoop 在 append 前会计算自己期待的序号，并检查 store.last_seq；
- EventStore 在自己的锁内重新分配最终 seq；
- AgentLoop 检查 store 返回的 seq 是否与期待值一致；
- JsonlEventStore 负责文件锁、半行检测、大小上限和缓存。

这也是为什么 EventStore::append 返回 Event，而不是只返回成功或失败。

## JSONL 为什么选择“一行一个事件”

JSONL 的每一行都是一个可以独立解析的 Event。它有几个直接好处：

- 追加新事件很简单；
- 崩溃后可以定位最后一行；
- 读日志时可以逐行验证；
- shell 和普通工具容易观察；
- 读取路径可以报告具体行号。

当前 JsonlEventStore 对文件有严格规则：

1. 空文件可以打开；
2. 完整行必须能解析成 Event；
3. 文件末尾没有换行时，读取报半行损坏；
4. 中间损坏行不能自动修复；
5. 每行 seq 必须与行号一致；
6. 超过事件数量或总字节上限要拒绝。

半行修复是显式命令 repair_partial_tail 的工作。读取时静默丢掉最后半行，会把“崩溃残留”误当成“没有发生过”。

## 一次 append 的事务顺序

src/runtime/agent_loop.rs 中的 append 是理解整个 runtime 的关键函数。它大致这样工作：

~~~text
旧 state
  │
  ├─ 计算 expected_seq
  ├─ 检查 store.last_seq
  ├─ 组装 event，检查事件大小
  ├─ clone 出 next_state
  ├─ reduce(next_state, event)
  │       └─ 失败：不写盘
  ├─ store.append(event)
  │       └─ 失败：状态不提交
  ├─ 检查 store 返回的 seq
  └─ self.state = next_state
~~~

“验证 → 持久化 → 提交”这条顺序解决两个风险：

- 非法事件不会污染日志；
- 内存状态不会领先于持久化事实。

但它不能让外部世界拥有 exactly-once 语义。文件已经改了、进程已经发出网络请求，随后日志追加失败，这个窗口会在第 7 章讨论。

## 用测试观察重放

运行：

~~~bash
cargo test jsonl_rejects_sequence_gaps_and_middle_corruption
cargo test jsonl_rejects_partial_tail
cargo test reducer_allows_recovery_input_after_failure_but_not_close
cargo test --test property_replay
~~~

阅读 property replay 测试时，关注两个断言：

- 同一事件序列重复 reduce，结果完全相等；
- 非法序列会返回错误，而不是 panic。

这两个性质分别对应“可恢复”和“可诊断”。

## 这一章读完后

你应该能从一条事件回答：

- 它会改变 SessionState 的哪些字段；
- 它要求当前状态满足什么前置条件；
- 它能否被重放；
- 它失败时应该阻止追加，还是生成一个终态事件。

## 思考题

1. 如果 reducer 在处理 ToolCompleted 时读取当前文件内容，会破坏什么性质？
2. 为什么 JsonlEventStore 不能在读取时自动丢掉末尾半行，却可以提供单独的 repair_partial_tail？
3. 如果 append 已经把事件写入文件，但进程在 self.state = next_state 之前崩溃，重启时会发生什么？
4. 为什么 SessionState 的 pending_inputs 要从事件推导，而不是只存在 actor 的 VecDeque 中？
5. 你会把 checkpoint 当成事实，还是当成缓存？如果 checkpoint 损坏，正确的退路是什么？

下一章：[MockProvider、Session 和 actor](03-mock-provider-and-session.md)。

