# 第 0 章：先建立一张地图

这一章不让你写代码。你要做的是用当前仓库跑出一条请求，然后知道每个文件在这条请求中扮演什么角色。

后面章节会反复回到这张地图。如果某一章看不懂，先回来问自己：它是在处理模型建议、工具副作用、事实记录，还是客户端显示？

## 一个只完成表面工作的 agent

下面的循环看起来很像 agent：

~~~python
while True:
    reply = llm(messages, tools=TOOLS)
    if reply.tool_call:
        result = run_shell(reply.tool_call.command)
        messages.append(result)
    else:
        print(reply.text)
        break
~~~

它能在理想环境下回答问题，但没有定义：

- Ctrl-C 是否杀掉子进程；
- 500 MB 输出是否会撑爆内存；
- 文件改了一半时崩溃怎样恢复；
- workspace 外的路径谁来拒绝；
- 两个调用同时修改同一个文件怎么办；
- UI 退出后会话是否还活着。

harness 负责的就是这些模型循环之外的系统行为。模型提出建议，harness 决定怎样验证、授权、执行、记录和恢复。

## 先运行当前项目

在仓库根目录运行：

~~~bash
cargo test
cargo run -- demo read README.md
~~~

demo 是 src/main.rs 中的 run_demo。它使用 MockProvider、LocalExecutor、ToolRegistry、DefaultPolicy 和 InMemoryEventStore，完成一次不联网的 read 工具调用。

它不会把事件写到持久化 JSONL 文件，所以适合第一次观察。

想看到完整事件：

~~~bash
cargo run -- demo read README.md --json
~~~

把输出先看成一条时间线：

~~~text
session.created
user.input.recorded
turn.started
provider.continuation.updated
model.response.recorded        模型提出 read
tool.requested
tool.started
tool.completed                  文件内容回到历史
provider.continuation.updated
model.response.recorded        模型给出最终文本
turn.completed
~~~

你不需要先理解每个字段。先确认这条因果链：

~~~text
用户输入
  → 模型提出工具调用
  → runtime 记录请求
  → policy 允许
  → executor 读取文件
  → 结果进入历史
  → 模型再次响应
  → turn 完成
~~~

## 八个核心名词

| 名词 | 当前项目中的含义 |
|---|---|
| Session | 一段持续的会话和它的事件历史 |
| Turn | 一次用户输入到最终回答、失败或取消 |
| Step | turn 中的一次模型请求和处理 |
| Provider | 把模型服务翻译成 ModelRequest 和 ModelResponse |
| Tool | 模型可以请求的操作，例如 read、edit、bash |
| Policy | 决定工具调用是 Allow、Ask 还是 Deny |
| Executor | 真正访问文件和进程的能力层 |
| Event | 已经发生的事实 |
| State | reducer 从事件推导出的当前视图 |

Session、Turn、Step 和 ToolExecution 是层次关系：

~~~text
Session
  └─ Turn
       ├─ Step
       │    └─ ToolExecution
       ├─ Step
       └─ ...
~~~

一个模型响应可以没有工具调用，也可以包含一个或多个工具调用。一个工具调用有模型的 ToolCallId，实际执行还有 ExecutionId。第 1 章会解释这些身份。

## 当前项目的模块地图

先打开 src/lib.rs，确认项目导出的模块：

~~~text
config
durable
error
executor
model
policy
protocol
runtime
scheduler
tools
ui
~~~

再按下面的角色理解每个目录：

~~~text
src/main.rs
  └─ CLI 入口和组件组装

src/runtime/
  ├─ agent_loop.rs       一次 turn 的主循环
  ├─ agent_loop_setup.rs 构造和恢复 AgentLoop
  ├─ session.rs          Session、SessionHandle、SessionCommand
  ├─ session_actor.rs    单写者命令循环
  ├─ context.rs          给模型的有限快照
  └─ state.rs            SessionState 和派生生命周期

src/model/
  ├─ provider.rs         Provider trait 和模型类型
  ├─ mock.rs             确定性 mock
  ├─ openai.rs           HTTP provider
  └─ openai_protocol.rs  两种 wire format 的转换

src/tools/
  ├─ spec.rs             Tool、ToolSpec、ToolContext
  ├─ registry.rs         工具注册和查找
  └─ read.rs/edit.rs/bash.rs

src/executor/
  ├─ trait.rs            文件、进程、编辑的能力接口
  ├─ local.rs            本地 executor
  ├─ process.rs          进程组、排空、输出上限
  └─ filesystem.rs       路径和原子编辑

src/durable/
  ├─ event.rs            Event 和 EventPayload
  ├─ reducer.rs          事件到状态
  ├─ store.rs             EventStore trait
  ├─ memory.rs            内存事件日志
  ├─ event_log.rs         JSONL 事件日志
  ├─ recovery.rs          崩溃分类
  └─ checkpoint.rs        快照和重放

src/protocol/
  ├─ jsonl_transport.rs  有界 JSONL 传输
  ├─ jsonl.rs             serve --stdio
  └─ event_projection.rs  安全事件投影
~~~

目录不是学习顺序。学习顺序应该跟着一条数据流：

~~~text
main.rs 组装组件
  → Session.start_turn
  → AgentLoop.run_turn_with_cancel
  → Provider.complete
  → ToolRegistry.lookup
  → Tool.execute
  → Executor
  → AgentLoop.append
  → EventStore
  → reducer
~~~

## 六条设计边界

### 模型边界

Provider 只看到 ModelRequest，返回 ModelResponse。它不修改 runtime state，也不决定权限。

### 工具边界

Tool 解释模型输入，生成模型可读结果。它通过 ToolContext 获得能力，不持有 Session 或 EventStore。

### 执行边界

Executor 处理文件和进程等外部副作用。它有结构化错误、取消、超时和输出限制。

### 持久化边界

Event log 保存事实。SessionState 是 reducer 产物，checkpoint 是加速重放的缓存。

### 并发边界

每个 session 的状态和追加由一个 actor 负责。其他线程或进程通过命令和 durable store 参与。

### 客户端边界

CLI、TUI 和 Ink UI 通过 JSONL 协议发命令和消费事件投影。它们不直接调用 provider 或 executor。

## 这一章的阅读动作

现在打开 src/main.rs 的 run_demo，按以下顺序跳转：

1. 找 MockProvider 的脚本；
2. 找三个工具如何注册；
3. 找 Session::new_with_policy；
4. 找 Session::start_turn；
5. 找 AgentLoop 的 begin_turn 和 continue_turn；
6. 找 tool completed 后 history 如何变化；
7. 找事件最终从 InMemoryEventStore 读出并打印的位置。

每跳到一个函数，只记两句话：

~~~text
它拿到什么？
它把什么交给下一层？
~~~

不要在这一步停留在 tokio、泛型或 serde 的细节。先弄清数据流，再在后面的章节回头理解实现细节。

## 第 0 章检查

你能做到下面四件事，就可以继续：

- 用一句话解释 harness 和“给模型加几个工具函数”的区别；
- 从 run_demo 追到 ReadTool 和 LocalExecutor；
- 说出 Event、State、Policy 和 Executor 各自负责什么；
- 从 demo 输出指出 tool.requested、tool.completed 和 turn.completed 的先后关系。

## 思考题

1. 为什么 UI 只消费事件投影，而不是直接拿 SessionState？
2. 如果 provider 直接执行 bash，Policy 和 Executor 还剩下什么意义？
3. 如果只保存 SessionState，不保存事件，崩溃恢复会失去什么？
4. 一个新的远程 executor 应该替换哪一层？为什么 AgentLoop 不应该跟着重写？
5. 如果允许两个工具并行执行，当前“单写者 + 顺序事件”要重新回答哪些问题？

下一章：[先读懂类型和事件](01-ids-and-events.md)。

