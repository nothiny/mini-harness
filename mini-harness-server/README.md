# mini-harness-server

mini-harness 的 JSONL 协议服务器（可选组件）。

主库刻意只提供单线程同步的 `Session` 接口；需要在 turn 运行中响应
取消/审批的客户端（例如 TUI）使用本 crate。它在 `Session` 之上组合了
`tokio::spawn` + `Mutex` + `CancellationToken`，演示了生产级 Actor
模式的最小形态。

## 使用

```bash
cargo build -p mini-harness-server
./target/debug/mini-harness-server --stdio
```

协议（v2，行分隔 JSON-RPC 2.0 风格，stdout 只归协议，诊断写 stderr）:

| 方法 | 参数 | 结果 |
|------|------|------|
| `session.create` | `{event_log?, workspace?, provider?, model?}` | `{session_id}` |
| `session.resume` | `{session_id, event_log?, ...}` | `{session_id}` |
| `turn.start` | `{input, session_id?}` | `{turn_id, accepted}` |
| `turn.wait` | `{turn_id, timeout_ms?}` | `{text, completed, events}` |
| `turn.cancel` | `{turn_id}` | `{cancelled}` |
| `approval.respond` | `{turn_id, call_id, approved}` | `{persisted, completed, text?}` |
| `session.shutdown` | `{}` | `{shutdown}` |

事件通知作为独立流持续推送（`{method: "event", params: {seq, event}}`），
事件本体是脱敏的安全投影，不携带工具原始输入/输出。

## 结构

- `src/jsonl.rs` — 服务器主体（会话管理、并发包装、事件泵）
- `src/projection.rs` — 安全事件投影（脱敏、有界摘要）
- `src/transport.rs` — 行级 I/O（1MB 消息上限）
