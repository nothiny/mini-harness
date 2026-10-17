# 尾声：把这份 Harness 读成一个系统

你现在已经沿着一条完整路径读过项目：

~~~text
用户输入
  → Session / actor
  → AgentLoop
  → Provider
  → ModelResponse
  → Policy
  → Tool
  → Executor
  → Event log
  → reducer
  → 下一次 ModelRequest
  → 最终回答
~~~

外围还有两条重要路径：

~~~text
崩溃
  → replay
  → RecoveryFinding
  → 人的决定
  → 恢复事实

UI / CLI
  → JSONL 协议
  → SessionCommand
  → 安全事件投影
~~~

这两条路径让项目从“可以调用模型”变成“可以长期运行、可以观察、可以恢复的 runtime”。

## 用四个问题检查自己是否读懂

### 问题一：谁拥有写权限

会话状态和事件追加只有 AgentLoop 所在的 actor 能修改。Provider、Tool、UI 和 Scheduler 都通过明确接口请求它们。

### 问题二：什么是事实

用户输入、模型响应、工具请求、工具开始和完成、审批、取消、恢复和 checkpoint marker 都是事件。SessionState、run_state、UI 显示内容和 queue 的当前视图都是从事实推导出来的。

### 问题三：哪里可能不知道

工具或 provider 的外部副作用可能已经发生，但结果还没有进入日志。OutcomeUnknown 和 RecoveryFinding 让这个窗口显式存在，系统不会用“失败”假装知道结果。

### 问题四：哪个模块可以替换

Provider 可以换成新的 wire format；Executor 可以换成 sandbox 或远程实现；UI 可以换成另一个客户端。只要它们保持当前 trait 和事件语义，AgentLoop 不需要重新理解这些外部差异。

## 回到源码做一次总复盘

建议重新打开这几个入口：

1. src/main.rs 的 run_demo：组件怎样被接线；
2. src/runtime/agent_loop.rs 的 append：一条事实怎样进入日志；
3. src/durable/reducer.rs 的 reduce：日志怎样变成状态；
4. src/runtime/session_actor.rs 的 run_actor：谁拥有唯一写权限；
5. src/protocol/event_projection.rs 的 project_event：哪些事实能安全地给客户端；
6. src/durable/recovery.rs 的 classify_state：崩溃后的状态如何分类。

再运行一次：

~~~bash
cargo run -- demo read README.md --json
cargo test --test integration_agent_loop
cargo test --test recovery_classify
cargo test --test scheduler
~~~

这一次不要只看测试通过。把 demo 的每条事件放在下面这张表里：

| 事件 | 它记录的事实 | reducer 改变的状态 | 下一步谁消费 |
|---|---|---|---|
| user.input.recorded | 用户说了什么 | history、pending_inputs | AgentLoop |
| tool.requested | 模型要求哪个工具 | Pending execution | Policy / Tool |
| tool.started | 工具真的开始 | Running execution | Executor |
| tool.completed | 工具返回了什么 | Completed、history | Provider |
| turn.completed | 最终回答是什么 | active_turn 清空 | Session / UI |

如果某个事件无法回答“谁产生、谁消费、状态怎样变化”，就回到对应章节继续读。

## 接下来读什么

当前代码已经为这些方向留下了边界：

- sandbox：把 workspace 保护从尽力而为提升到操作系统能力；
- PTY：支持交互式进程、终端尺寸和实时输出；
- MCP：让工具从外部服务动态注册；
- 远程 executor：把 Executor trait 换成网络实现；
- 多 agent：让多个 Session 通过协议协作；
- property tests：随机生成合法事件序列，验证 reducer 重放；
- metrics：统计 turn latency、provider error、approval wait 和 unknown outcome。

选择下一步时，继续使用本教程的阅读方法：先找事实和边界，再找失败路径，最后让测试固定语义。

## 最后的思考题

1. 新 provider 增加一种工具调用协议时，哪些文件应该变化，哪些文件应该保持不动？
2. 如果把 event log 换成数据库，哪些 reducer 和恢复语义仍然必须保留？
3. 如果 UI 要显示实时 stdout，哪一条安全边界会被重新设计？
4. 如果允许并行工具调用，事件顺序、取消和并发上限需要新增什么事实？
5. 哪一个模块最容易变成“第二个 runtime”？你会用什么接口边界阻止它？
6. 你能否从一次 tool.outcome_unknown 事件，说明系统知道什么、不知道什么、下一步由谁决定？

到这里，教程的目标就完成了：你可以从用户输入追到外部副作用，再从事件日志反向追到恢复状态，也能判断一个新功能应该放在哪一层。

