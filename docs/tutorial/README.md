# mini-harness 教程：从一次请求读懂 Agent Harness

这是一份**对照源码学习**的教程。目标不是让你背下几个术语，而是让你能打开这个仓库，沿着一次真实请求走完代码，并解释每一个边界为什么存在。

你不需要先了解 Agent、事件溯源或分布式系统。只要会一点 Rust，愿意边读边运行命令，就可以从第 0 章开始。

## 这不是重写一个 Harness

你要学习的是当前仓库里的这份实现。教程不会要求你新建项目、复制一套代码，或在每章末尾得到一个“自己的 mini-harness”。

每章都采用同一个方法：

1. 先从一个用户动作或一次故障开始；
2. 找到当前代码处理它的入口；
3. 顺着数据、事件和状态往下读；
4. 用现有测试或一个只读命令验证理解；
5. 再解释为什么这个边界必须这样设计。

教程中的“任务”统一指一次源码阅读、运行观察或测试验证。

## 先用一句话理解 Harness

模型只负责提出建议：回答用户，或者请求调用某个工具。

Harness 负责把建议变成**受控的现实动作**：检查权限、执行文件或进程、记录发生过的事实、处理取消与崩溃，并把结果重新交给模型。

可以把它想成一个有门卫、有账本、有急停按钮的工作台：

```text
用户输入
   │
   ▼
Session / Turn ──▶ Provider（模型）──▶ Tool call（建议）
   │                                      │
   │                              Policy（能不能做）
   │                                      │
   │                              Executor（实际去做）
   │                                      │
   └──────────── Event log（发生过什么）◀─┘
```

## 这份教程怎样和源码对应

教程中的“第 3 章”等于当前仓库中的一个主题，不代表源码提交历史中的一天。每章都会明确列出：

1. 先读哪些真实文件；
2. 一次调用从哪个函数进入、经过哪些函数；
3. 运行哪条命令或测试观察结果；
4. 哪些代码是核心，哪些是为生产边界补上的细节。

### 源码地图

| 你要理解的东西 | 当前源码 | 先看什么 |
|---|---|---|
| 公共模块和导出 | [src/lib.rs](https://github.com/nothiny/mini-harness/blob/master/src/lib.rs) | 项目有哪些边界 |
| ID、历史、会话状态 | [src/runtime/ids.rs](https://github.com/nothiny/mini-harness/blob/master/src/runtime/ids.rs)、[src/runtime/state.rs](https://github.com/nothiny/mini-harness/blob/master/src/runtime/state.rs) | `SessionState`、`TurnState` |
| 事件模型 | [src/durable/event.rs](https://github.com/nothiny/mini-harness/blob/master/src/durable/event.rs) | `EventPayload`、`Event` |
| reducer | [src/durable/reducer.rs](https://github.com/nothiny/mini-harness/blob/master/src/durable/reducer.rs) | `reduce` 如何拒绝非法序列 |
| 事件存储 | [src/durable/store.rs](https://github.com/nothiny/mini-harness/blob/master/src/durable/store.rs)、[src/durable/event_log.rs](https://github.com/nothiny/mini-harness/blob/master/src/durable/event_log.rs) | trait 与 JSONL 实现 |
| 模型抽象 | [src/model/provider.rs](https://github.com/nothiny/mini-harness/blob/master/src/model/provider.rs) | `ModelRequest`、`ModelResponse`、`ModelProvider` |
| 主循环 | [src/runtime/agent_loop.rs](https://github.com/nothiny/mini-harness/blob/master/src/runtime/agent_loop.rs) | `append`、`run_turn_with_cancel` |
| 恢复和构造 | [src/runtime/agent_loop_setup.rs](https://github.com/nothiny/mini-harness/blob/master/src/runtime/agent_loop_setup.rs) | `restore`、`restore_with_checkpoint` |
| actor | [src/runtime/session.rs](https://github.com/nothiny/mini-harness/blob/master/src/runtime/session.rs)、[src/runtime/session_actor.rs](https://github.com/nothiny/mini-harness/blob/master/src/runtime/session_actor.rs) | 命令如何串行化 |
| 工具 | [src/tools/](https://github.com/nothiny/mini-harness/tree/master/src/tools/) | `Tool`、`ToolRegistry`、read/edit/bash |
| 执行器 | [src/executor/](https://github.com/nothiny/mini-harness/tree/master/src/executor/) | 文件和进程的真实副作用 |
| 协议 | [src/protocol/jsonl.rs](https://github.com/nothiny/mini-harness/blob/master/src/protocol/jsonl.rs) | 请求循环、事件通知、安全投影 |
| UI | [src/ui/](https://github.com/nothiny/mini-harness/tree/master/src/ui/) | UI 如何只消费协议 |
| CLI 组装 | [src/main.rs](https://github.com/nothiny/mini-harness/blob/master/src/main.rs) | 各个模块怎样接起来 |

## 十分钟预览：先看一条真实链路

先不要从类型定义开始。先让程序跑一次离线演示：

```bash
cargo run -- demo read README.md
```

这个命令使用 `src/main.rs::run_demo` 组装组件：

- `MockProvider` 按脚本返回一个 `read` 调用，再返回文本 `read complete`；
- `ToolRegistry` 注册 `ReadTool`、`EditTool`、`BashTool`；
- `LocalExecutor` 把工作区设为当前目录；
- `InMemoryEventStore` 保存事件，所以不会改动磁盘上的会话日志；
- `Session::start_turn` 启动一次 turn。

把输出看成一条时间线，大致会得到：

```text
session.created
user.input.recorded
turn.started
provider.continuation.updated
model.response.recorded        # 模型先提出 read
tool.requested
tool.started
tool.completed                  # 文件内容回到历史
provider.continuation.updated
model.response.recorded        # 模型最后说 read complete
turn.completed
```

这几行已经包含了全项目的核心思想：**模型的建议、工具的执行结果、最终回答，全部先成为事件，状态再从事件中推导出来。**

想看完整 JSON，可以运行：

```bash
cargo run -- demo read README.md --json
```

第一次看到很长的工具输出是正常的。先只关注每个对象的 `seq`、`payload.type` 和 `turn_id`；字段含义会在第 1、2 章逐个解释。

## 你会学到什么

| 章节 | 主题 | 读完后能回答的问题 |
|---|---|---|
| 第 0 章 | 先建立一张地图 | 模型、工具、权限和持久化分别是谁负责？ |
| 第 1 章 | 先读懂类型和事件 | 为什么不同 ID 不能都用 `String`？ |
| 第 2 章 | EventStore 和 reducer——日志怎样变成状态 | 重启后状态怎样从日志恢复？ |
| 第 3 章 | MockProvider、Session 和 actor | 怎样不联网先验证主循环？ |
| 第 4 章 | Tool、Policy 和 Executor——一次 read 怎样到达文件系统 | 参数检查、权限检查和真实读文件为什么分层？ |
| 第 5 章 | AgentLoop——一轮 turn 怎样反复采样 | 工具结果怎样回到下一次模型请求？ |
| 第 6 章 | 进程、取消和原子编辑 | 怎样避免进程泄漏、管道死锁和半个文件？ |
| 第 7 章 | 崩溃恢复——系统怎样面对“不知道” | 为什么“失败”和“结果未知”不能混为一谈？ |
| 第 8 章 | 真实 provider——Responses 与 Chat Completions | provider 换了，主循环为什么不用改？ |
| 第 9 章 | CLI 和 JSONL 协议——把 runtime 放到进程边界外 | 为什么 stdout 只能输出协议？ |
| 第 10 章 | 事件查看器和 Ink TUI——UI 只是事件投影 | UI 崩溃时，runtime 怎样继续工作？ |
| 第 11 章 | Scheduler、durable 队列和全局并发 | 输入队列怎样做到可恢复？并发上限放在哪里？ |
| 尾声 | 把这份 Harness 读成一个系统 | 怎样继续做 sandbox、PTY、MCP 或远程执行器？ |

术语不确定时，查[术语表](glossary.md)。

## 每章的阅读顺序

不要一开始就试图理解整章所有细节。每章按下面顺序读：

1. **先看本章问题**：知道这一章要解释哪个运行时行为；
2. **再看源码对照**：打开列出的文件，先找结构体和入口函数；
3. **运行观察命令**：把抽象概念变成输出或事件；
4. **逐步读主路径**：只追一条成功路径；
5. **最后读失败路径和测试**：理解边界是怎样被锁住的；
6. **再做思考题**：用自己的话解释“如果删掉这条检查会发生什么”。

每章的代码片段用于帮助你定位源码和理解关键步骤，不是另一份独立实现。源码中已经存在的实现才是最终事实；如果教程文字和源码不一致，以源码和对应测试为准，并把差异记下来。

文中的代码片段用于定位源码和解释关键步骤。先不要复制代码；只有当你想检验自己是否真的掌握时，才把某个函数改成小实验，再用测试恢复。

## 环境和常用命令

要求：Rust 1.85+，2024 edition。

```bash
rustc --version
cargo test
cargo test --test integration_agent_loop
cargo test --test recovery_classify
RUST_LOG=mini_harness=debug cargo run -- demo read README.md
```

`cargo test` 是离线的。只有第 8 章的真实 provider 部分需要 API key；教程会先用 mock HTTP server 讲清楚协议，因此不必一开始准备密钥。

## 学习中的三个约定

### 1. 看到 `Event`，先问它记录的是事实还是状态

`tool.completed` 是事实；“当前有没有正在运行的工具”是 reducer 推导出来的状态。这个区分会贯穿恢复、协议和 UI。

### 2. 看到 `Arc`，先问谁拥有写权限

共享读取很常见；会话状态和事件追加只有 actor 能改。后面看到 `SessionCommand`，把它理解为“请求唯一写者替我做一次状态迁移”。

### 3. 看到 `Result`，先问错误由谁处理

工具参数错误可以回给模型；策略拒绝要显示给用户；事件日志损坏通常要停止恢复。错误类型的边界就是模块边界的说明书。

准备好后，从[第 0 章：先建立一张地图](00-overview.md)开始。
