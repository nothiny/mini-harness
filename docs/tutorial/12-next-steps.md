# 尾声：把这份 Harness 读成一个系统

你现在已经沿着一条完整路径读过项目：

~~~text
用户输入
  → Session（单写者门面）
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
  → 完整重放（无 checkpoint）
  → 活跃 turn / 挂起审批浮出水面
  → 人的决定（取消或批准）

UI / CLI
  → mini-harness-server（JSONL 协议）
  → begin_turn / continue_turn 拆分
  → 安全事件投影
~~~

这两条路径让项目从“可以调用模型”变成“可以长期运行、可以观察、可以恢复的 runtime”。

## 用四个问题检查自己是否读懂

### 问题一：谁拥有写权限

同一个 Session 同一时刻只有一个驱动者（`&mut` 借用强制）。Provider、Tool 和 UI 都拿不到写权限；跨进程写入靠 append 前的 last_seq 校验拦截。

### 问题二：什么是事实

用户输入、模型响应、工具请求和结果、审批决定、取消、超时和 provider attempt 都是事件。SessionState、UI 显示内容和 pending_input 槽都是从事实推导出来的。

### 问题三：哪里可能不知道

工具或 provider 的外部副作用可能已经发生，但结果还没有进入日志。OutcomeUnknown 让这个窗口显式存在，系统不会用“失败”假装知道结果。

### 问题四：哪个模块可以替换

Provider 可以换成新的 wire format；Executor 可以换成 sandbox 或远程实现；UI 可以换成另一个客户端。只要它们保持当前 trait 和事件语义，AgentLoop 不需要重新理解这些外部差异。

## 回到源码做一次总复盘

建议重新打开这几个入口：

1. src/main.rs 的 run_demo：组件怎样被接线；
2. src/runtime/agent_loop.rs 的 append：一条事实怎样进入日志；
3. src/durable/reducer.rs 的 reduce：日志怎样变成状态；
4. src/runtime/session.rs 与 agent_loop_setup.rs 的 restore：单写者门面与崩溃重放；
5. mini-harness-server/src/projection.rs 的投影：哪些事实能安全地给客户端；
6. mini-harness-server/src/jsonl.rs 的 turn.start：外层怎样组合出并发。

再运行一次：

~~~bash
cargo run -- demo read README.md --json
cargo test --test integration_agent_loop
cargo test --test cross_process_cancel
cargo test --test property_replay
~~~

这一次不要只看测试通过。把 demo 的每条事件放在下面这张表里：

| 事件 | 它记录的事实 | reducer 改变的状态 | 下一步谁消费 |
|---|---|---|---|
| user.input.recorded | 用户说了什么 | history、pending_input | AgentLoop |
| tool.requested | 模型要求哪个工具、是否需审批 | Pending execution（可带 Pending 审批） | Policy / Tool |
| tool.completed | 工具结果或失败 | 终结 execution、history | Provider |
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

