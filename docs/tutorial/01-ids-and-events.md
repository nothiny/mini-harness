# 第 1 章：先读懂类型和事件

这一章回答一个问题：**这份 harness 用什么类型描述“谁在做什么”，又用什么事件记录“已经发生了什么”？**

你暂时不需要理解异步，也不需要理解模型 API。先把项目的词汇读懂，后面每个模块都会复用这些词汇。

## 这一章在整条链路中的位置

一次工具调用至少涉及四个身份：

~~~text
SessionId    哪个会话
TurnId       哪一轮用户请求
ToolCallId   模型提出的哪一次工具调用
ExecutionId  runtime 实际启动的哪一次执行
~~~

它们都会出现在事件里，但含义不同。把四个 ID 都写成 String，代码可以编译，错误却会在运行时才暴露。当前项目在 src/runtime/ids.rs 里用同一个宏生成这些 newtype。

先打开这个文件的底部。你会看到 SessionId、TurnId、StepId、ToolCallId、ExecutionId、EventId 和 TaskId。每个类型内部都包着一个 UUID，但显示和序列化时会带前缀，例如 sess_、turn_、call_。前缀是给日志和人看的，newtype 是给编译器看的。

## 从一个 ID 读出 Rust 的边界意识

src/runtime/ids.rs 中的宏为每个 ID 提供了四类能力：

1. new：生成随机 UUID；
2. Display：打印带前缀的字符串；
3. parse 和 FromStr：拒绝错误前缀或坏 UUID；
4. Serialize 和 Deserialize：JSON 中仍然保持同样的格式。

因此协议收到 turn_... 时，解析成 TurnId；同一个字符串不会因为“长得像”就自动成为 ToolCallId。

src/runtime/types.rs 放的是更高层的值类型：

| 类型 | 表示什么 | 常见位置 |
|---|---|---|
| UserInput | 用户给出的文字 | history 和 user.input.recorded |
| ModelText | 模型产生的文字 | assistant history 和最终回答 |
| ToolResult | 工具给模型的结果 | tool history |
| ToolName | 工具注册名 | ToolSpec、ToolCall |
| EventSeq | 会话内单调递增的序号 | Event |
| ByteLimit | 一个字节上限 | 工具和执行器配置 |

这些类型不一定解决安全问题，但能让函数签名说清楚数据的用途。看到一个 String 时，你要问它是不是应该变成更明确的类型。

## Event 有两层

打开 src/durable/event.rs。EventPayload 表达“发生了什么”，Event 表达“这件事属于哪个会话、发生在什么时候、排在第几条”。

~~~text
EventPayload
  └─ SessionCreated
  └─ UserInputRecorded
  └─ TurnStarted
  └─ ToolRequested
  └─ ToolStarted
  └─ ToolCompleted
  └─ TurnCompleted
  └─ ...

Event
  ├─ schema_version
  ├─ event_id
  ├─ seq
  ├─ timestamp
  ├─ session_id
  ├─ turn_id
  └─ payload
~~~

把它类比成账本：

- payload 是“取款”或“存款”这类事实；
- seq 是账本行号；
- session_id 是账户；
- event_id 是这条记录自己的身份证；
- timestamp 是观测信息；
- turn_id 说明这条事实属于哪一次用户请求。

Reducer 只根据事件内容和顺序改变状态。它不能因为当前时间、网络结果或随机数而得到不同结果。

## 从 demo 找到事件

运行：

~~~bash
cargo run -- demo read README.md --json
~~~

把 JSON 数组中每个对象的 payload.type 记下来。当前项目的最小 read 演示通常会出现：

~~~text
session_created
user_input_recorded
turn_started
provider_continuation_updated
model_response_recorded
tool_requested
tool_started
tool_completed
provider_continuation_updated
model_response_recorded
turn_completed
~~~

这里的两个 model_response_recorded 分别对应模型提出工具调用和模型给出最终文本。provider_continuation_updated 保存 provider 续传所需的事实；它仍然属于同一个 turn。

## 错误类型也在描述边界

打开 src/error.rs。HarnessError 把错误按处理者分组：

- Durable：事件日志或 checkpoint 有问题；
- Provider：模型服务或响应格式有问题；
- Tool：工具参数或模型可见的工具错误；
- Execution：文件、进程和操作系统错误；
- Policy：策略拒绝；
- ApprovalPending：等待用户决定；
- Cancelled 和 Timeout：控制流；
- InvariantViolation：代码认为不可能发生的状态；
- QueueLimitExceeded：输入队列已满。

读到一个错误时，先问“谁能处理它”。模型可以看到工具输出形式的错误，用户可以批准或拒绝工具，runtime 必须停止处理损坏的日志。

## 对照测试阅读

先运行：

~~~bash
cargo test ids_are_round_tripable_and_type_scoped
cargo test jsonl_reopens_and_replays
~~~

然后打开对应测试，按这个顺序看：

1. 测试创建了哪一种 ID；
2. 它打印和解析的字符串是什么；
3. 它如何构造 Event；
4. 它最后断言了哪些字段相等。

测试不是额外知识，它是当前实现对这些类型做出的承诺。

## 你这一章应该能说清楚

- ToolCallId 为什么不能传给只接受 ExecutionId 的函数；
- EventPayload 和 Event 的职责差异；
- timestamp 为什么可以进事件，但 reducer 不应读取它；
- 一个错误为什么要区分 Policy、Provider、Execution 和 Durable。

## 思考题

1. 如果把所有 ID 改成 String，哪一类错误会从编译期推迟到运行期？请举一个具体函数调用。
2. 为什么 EventId 和 seq 都存在？如果只保留其中一个，诊断或并发追加会失去什么信息？
3. ToolResult 和 ModelText 都包着 String。你会在哪些函数边界拒绝它们互换？
4. 一个 provider 自己返回的 call_id 应该在哪一层转换成 ToolCallId？为什么不让 reducer 负责？
5. timestamp 被篡改会影响状态重放吗？它会影响哪些诊断能力？
 
下一章：[EventStore 和 reducer——日志怎样变成状态](02-event-store-and-reducer.md)。

