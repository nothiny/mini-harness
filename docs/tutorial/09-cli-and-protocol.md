# 第 9 章：CLI 和 JSONL 协议——把 runtime 放到进程边界外

这一章解释：为什么 TUI、TypeScript 客户端和其他程序不能直接共享 AgentLoop，而要通过 stdio JSONL 与一个独立的协议服务器进程通信。

进程边界会强迫系统明确三个问题：

- stdout 上什么是协议，什么是诊断；
- 请求和异步事件如何同时发送；
- UI 断开时，runtime 的会话是否继续。

## CLI 负责组装，库负责语义

打开 `src/main.rs`。main 的主要工作是：

~~~text
解析 clap 参数
  → 加载 TOML 配置
  → 初始化 stderr tracing
  → 选择 provider、executor、tools、policy、store
  → 调用库中的 Session
~~~

main.rs 不应该包含 reducer 规则、工具执行细节。这些属于库模块，所以测试可以直接调用库，也可以启动真正的二进制验证进程边界。

CLI 命令按用途分成三类：

| 类别 | 命令 |
|---|---|
| 运行会话 | `run`、`resume`、`cancel` |
| 查看与审计 | `events`、`inspect`（含 `--until <seq>`）、`sessions` |
| 工具与诊断 | `demo`、`config`、`doctor` |

注意 `events` 和 `inspect` 直接读事件日志，不启动模型也不联网——它们是纯审计命令，配合 `--format raw` 可以看到未脱敏的完整事件（仅限可信本地环境）。

## 协议服务器是一个独立的 crate

简化版把协议服务器从主库里拆了出去：`mini-harness-server/`。

~~~bash
cargo run -p mini-harness-server -- --stdio   # 或 make serve
~~~

主库 `mini-harness` 保持零协议依赖；想嵌进自己的程序时不需要拖上 JSONL 服务器。服务器自己也是一个薄壳：组装 Session，把 stdin/stdout 翻译成方法调用（`mini-harness-server/src/jsonl.rs`）。

## JSONL transport 的两个核心承诺

打开 `mini-harness-server/src/transport.rs`。它处理的是字节流，不是"每次 read_line 就完事"。

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

打开 `mini-harness-server/src/jsonl.rs`。协议有两个方向：

~~~text
stdin
  → 请求循环
      → protocol.negotiate
      → session.create / session.resume
      → turn.start / turn.wait / turn.cancel
      → approval.respond
      → session.shutdown
      → 一个 request id 对应一个 response

event log
  → 事件泵（tokio::spawn(event_pump …)）
      → event notification（安全投影，带 seq）
      → 客户端按 seq 去重、检测缺失
~~~

`turn.start` 的实现值得细读：它调用 `Session::begin_turn` 立即持久化输入并返回 turn_id，然后 `tokio::spawn(drive_turn(...))` 在后台驱动 `continue_turn`。请求循环不等 turn 跑完——turn 可能要几十秒，还可能停下来等审批。

`turn.wait { turn_id, timeout_ms? }` 返回 `{ text, completed }`；超时返回错误码 `turn_pending`；turn 停在审批时返回 `approval_pending`。错误码是稳定契约，客户端据此决定下一步（等待、问用户、放弃）。

## 典型协议路径

~~~text
protocol.negotiate
  → 协商版本与能力

session.create
  → 返回 session_id

turn.start
  → 返回 turn_id（输入已持久化，执行已在后台开始）

（事件泵持续推送 notification）
  → tool.requested / tool.completed / turn.completed ...

turn.wait
  → 完成时返回最终文本
  → timeout 时返回 turn_pending
  → 停在审批时返回 approval_pending

approval.respond { turn_id, call_id, approved }
  → 决定落盘后返回 persisted
~~~

`approval.respond` 只有在决策真正追加到事件日志后才报告成功。客户端拿到成功响应，就可以相信这个决定已经成为事实。

## 安全事件投影

durable Event 中的 ToolRequested 可能包含完整 command、old_text 或工具输入。协议通知不能直接把完整 payload 广播出去。

打开 `mini-harness-server/src/projection.rs`。v2 projection 保留白名单字段，例如：

~~~text
type
event_id
timestamp
schema_version
tool_call_id
tool
requires_approval
approved
input_summary          单行摘要（100 字符级）
input_detail           结构化摘要：bash 的 command、read/edit 的 path，
                       edit 另带 old/new 的有界预览
result_summary         工具结果元数据（exit code、字节数、hash、行号）
error_summary          工具失败原因摘要
~~~

input_summary / input_detail 是给审批界面看的短摘要：

- bash 显示 command 的有限前缀；
- read 和 edit 显示 path，edit 另带旧/新文本的有界预览；
- 其他输入显示 opaque input；
- 摘要带 truncated / redacted 标志，常见凭据（api key、token、password 等）会被脱敏；
- 工具结果只投影元数据（exit code、字节数、hash、行号），stdout / stderr 原始文本不进投影；
- 完整输入与输出仍留在 event log 里。

这是一个实际的信任边界：客户端能显示"正在请求什么"，但不自动获得所有工具原始内容。

## 跨进程 cancel 的故事

cancel 可以由另一个进程打开同一个 event log 并追加 `turn.cancel_requested` 和 `turn.cancelled`（`mini-harness cancel`，或对服务器发 `turn.cancel`）。正在运行的进程下一次 append 时发现 `store.last_seq` 已经变化，就会以 out-of-sync 停止，避免覆盖另一个 writer 的事实。

这会留下一个现实窗口：

~~~text
工具可能已经产生副作用
当前进程却无法把结果追加进同一日志
~~~

崩溃恢复的视角（第 7 章）负责解释这个窗口。协议层不能把它伪装成普通成功。

## 读协议测试

~~~bash
cargo test -p mini-harness-server --test stdio_protocol
cargo test -p mini-harness-server --test protocol_compat
cargo test --test jsonl_cross_process
cargo test --test cli
~~~

先看坏 JSON 和超长行，再看 create → start → wait，最后看跨进程 cancel。测试顺序对应协议从输入可靠性到业务语义的层次。

## 思考题

1. stdout 混入一行 tracing 日志时，为什么客户端无法"跳过它继续解析"？
2. 为什么超长行必须被完整吞掉，而不是立即返回错误？
3. `turn.start` 为什么必须"先持久化输入再返回 turn_id"，而不是"先返回再落盘"？
4. 请求 response 和事件 notification 都写 stdout，怎样保证一条 JSON 不会被另一条写到一半插入？
5. UI 想显示完整 bash 输出，应该直接扩大安全投影，还是增加受控读取接口？
6. 跨进程 cancel 的 out-of-sync 保护避免了什么，恢复流程又要补上什么？

下一章：[Ink TUI——UI 只是事件投影](10-ui.md)。
