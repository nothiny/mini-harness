# 第 9 章：CLI 和 JSONL 协议——把 runtime 放到进程边界外

这一章解释：为什么 TUI、TypeScript 客户端和其他程序不能直接共享 AgentLoop，而要通过 stdio JSONL 和 session actor 通信。

进程边界会强迫系统明确三个问题：

- stdout 上什么是协议，什么是诊断；
- 请求和异步事件如何同时发送；
- UI 断开时，runtime 的会话是否继续。

## CLI 负责组装，库负责语义

打开 src/main.rs。main 的主要工作是：

~~~text
解析 clap 参数
  → 加载 TOML 配置
  → 初始化 stderr tracing
  → 选择 provider、executor、tools、policy、store
  → 调用库中的 Session、recovery 或 protocol
~~~

main.rs 不应该包含 reducer 规则、工具执行细节或恢复分类。这些属于库模块，所以测试可以直接调用库，也可以启动真正的二进制验证进程边界。

CLI 命令可以按用途分成三类：

| 类别 | 命令 |
|---|---|
| 运行会话 | run、resume、cancel |
| 协议和 UI | serve、events、tui |
| 恢复和诊断 | inspect、recover、abandon-turn、sessions、doctor |

## JSONL transport 的两个核心承诺

打开 src/protocol/jsonl_transport.rs。它处理的是字节流，不是“每次 read_line 就完事”。

write_json_line 要保证：

- 序列化成一行；
- 不超过单行上限；
- write_all 完整写入；
- flush；
- 不能把诊断日志写到 stdout。

read_bounded_line 要保证：

- 逐块寻找换行；
- 超过上限时标记 TooLarge；
- 继续读取直到整行结束；
- 下一条消息不会从上一条超长消息的半截开始。

因此坏 JSON、超长输入或缺少换行不会污染后续请求。

## 请求响应和事件通知

打开 src/protocol/jsonl.rs。协议服务器有两个方向：

~~~text
stdin
  → 请求循环
      → session.create / turn.start / turn.wait / ...
      → 一个 request id 对应一个 response

event store
  → 事件泵
      → event notification
      → 带 seq，客户端可以去重和检测缺失
~~~

请求响应回答“我发的命令是否被接受或完成”。事件通知回答“会话后来发生了什么”。turn 运行时间可能很长，不能让请求循环只靠一个阻塞响应传递全部进度。

## 典型协议路径

~~~text
session.create
  → 返回 session_id

turn.start
  → 返回 turn_id 和 queued 状态

event notification
  → tool.started / approval.requested / turn.completed ...

turn.wait
  → 完成时返回最终文本
  → timeout 时返回 turn_pending
  → 失败时返回稳定错误码
~~~

Approval.respond 只有在决策真正追加到事件日志后才应该报告 persisted。客户端拿到成功响应，就可以相信这个决定已经成为事实。

## 安全事件投影

durable Event 中的 ToolRequested 可能包含完整 command、old_text 或工具输入。协议通知不能直接把完整 payload 广播出去。

打开 src/protocol/event_projection.rs。v2 projection 保留白名单字段，例如：

~~~text
type
event_id
timestamp
schema_version
tool_call_id
execution_id
tool
approved
reason
input_summary
~~~

input_summary 是给审批界面看的短摘要：

- bash 显示 command 的有限前缀；
- read 和 edit 显示 path 或 text edit；
- 其他输入显示 opaque input；
- 完整输入仍留在 event log。

这是一个实际的信任边界：客户端能显示“正在请求什么”，但不自动获得所有工具原始内容。

## 跨进程 cancel 的故事

cancel 可以由另一个进程打开同一个 event log 并追加 turn.cancel_requested 和 turn.cancelled。正在运行的进程下一次 append 时发现 store.last_seq 已经变化，就会以 out-of-sync 停止，避免覆盖另一个 writer 的事实。

这会留下一个现实窗口：

~~~text
工具可能已经产生副作用
当前进程却无法把结果追加进同一日志
~~~

恢复命令负责分类这个窗口。协议层不能把它伪装成普通成功。

## 读协议测试

~~~bash
cargo test --test stdio_protocol
cargo test --test protocol_compat
cargo test --test jsonl_cross_process
cargo test --test cli
~~~

先看坏 JSON 和超长行，再看 create → start → wait，最后看跨进程 cancel。测试顺序对应协议从输入可靠性到业务语义的层次。

## 思考题

1. stdout 混入一行 tracing 日志时，为什么客户端无法“跳过它继续解析”？
2. 为什么超长行必须被完整吞掉，而不是立即返回错误？
3. 请求 response 和事件 notification 都写 stdout，怎样保证一条 JSON 不会被另一条写到一半插入？
4. UI 想显示完整 bash 输出，应该直接扩大安全投影，还是增加受控读取接口？
5. UI 在 turn.start 后断开连接，runtime 应该自动 cancel 吗？你会把“连接断开”和“用户取消”记录成同一个事实吗？
6. 跨进程 cancel 的 out-of-sync 保护避免了什么，恢复流程又要补上什么？

下一章：[事件查看器和 TUI——UI 只是事件投影](10-ui.md)。

