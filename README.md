# mini-harness

从零实现的 agent harness 运行时：**可取消、可崩溃恢复、可接真实模型**。

harness 是模型循环之外的全部系统——工具执行与授权、进程管理、事件日志、崩溃恢复、协议与 UI。本项目把这些边界逐阶段砌了出来，并用一组无网络依赖的测试钉住核心语义。

## 特性

- **事件溯源内核**：JSONL 事件日志是唯一事实，确定性 reducer 从事实重建全部状态；重放两次必然同态
- **单写者 session actor**：状态迁移与事件追加由唯一写者完成，先验证、再持久化、后提交
- **真实的进程执行**：进程组级取消（连子孙进程一起清理）、stdout/stderr 并发排空、三层输出上限、UTF-8 安全截断
- **崩溃恢复语义**：区分"确定失败"与 `OutcomeUnknown`（可能已产生副作用）；原子 checkpoint，损坏时回退全量重放
- **durable 输入队列**：turn 运行期间的输入作为事实排队，按 FIFO 自动执行，队列上限拒绝先于持久化
- **全局进程并发上限**：executor 层共享信号量，超限返回结构化错误而非无限排队
- **provider 可替换**：确定性 MockProvider 与 OpenAI Responses / Chat Completions 通过同一套行为契约测试
- **JSONL 控制协议**：stdout 只归协议；v2 事件通知是安全投影，工具原始输入/输出不出协议层
- **TOML 配置**：五个 section，未知字段按行号拒绝，分层 CLI > 文件 > 默认值

## 架构

```text
CLI / Ink UI ──(JSONL over stdio)──▶ Session actor（单写者）
                                       │
                 ┌─────────────────────┼──────────────────┐
            Provider              Agent loop          Policy
          mock / OpenAI        （事务性 append）    allow/ask/deny
                 └─────────────────────┼──────────────────┘
                                       │
                                  Executor
                  workspace 边界 · 进程组 · 输出上限 · 并发信号量
                                       │
                        Event log（事实）+ Checkpoint（快照）
```

## 快速开始

要求：Rust 1.85+（2024 edition）。

```bash
git clone https://github.com/nothiny/mini-harness.git
cd mini-harness
cargo test                        # 全部离线
cargo run -- demo read README.md  # 离线观察一次完整工具调用链
cargo run -- run "hello"          # 持久化会话（默认 mock provider）
```

### CLI 一览

| 命令 | 说明 |
|---|---|
| `run` / `resume` / `cancel` | 持久化会话；省略 `--event-log` 时使用 `~/.mini-harness/sessions/<id>/` 目录布局 |
| `serve --stdio` | JSONL 协议服务器（`session.create`、`turn.start/wait/cancel`、`approval.respond`…） |
| `tui` / `events` | 交互式 UI / 一次性事件查看器，只消费安全事件投影 |
| `inspect` / `recover` / `abandon-turn` / `sessions` | 恢复工具：分类未完成工作、标记未知结果、显式放弃 |
| `init` / `config` / `doctor` | 写默认配置 / 打印生效配置 / 环境体检 |
| `demo read` / `demo edit-bash` | 内存事件日志的离线演示，不联系真实 provider |

### 接入 DeepSeek

```bash
export DEEPSEEK_API_KEY=sk-...
cargo run -- run "总结一下这个项目" --provider deepseek --workspace .
```

或在配置文件里固定（对 `run`/`resume`/`tui`/`events`/`serve` 全部生效）：

```toml
[model]
provider = "deepseek"
name = "deepseek-flash"
```

`run` 是非交互命令。默认策略下 `edit_workspace` 和 `bash` 都是 `ask`，一旦模型要写文件或执行命令，`run` 会以 `approval pending` 结束而不会执行工具。无人值守场景在配置里放宽：

```toml
[permissions]
read_workspace = "allow"
edit_workspace = "allow"
bash = "allow"   # 模型可不经确认执行任意命令，仅限受控环境
```

说明：

- DeepSeek 走 OpenAI 兼容的 **Chat Completions** 协议；与 OpenAI Responses 是两种 wire 格式，本项目按 provider 自动选择
- `deepseek-flash` 支持函数调用（read/edit/bash 工具链可用）
- `serve --stdio` 的 session 参数同样接受 `provider: "deepseek"`；`doctor` 会检查 `DEEPSEEK_API_KEY`
- 其他 Chat Completions 兼容服务（Kimi、Qwen、vLLM、Ollama……）可通过 `OpenAiProviderConfig::with_endpoint(...).with_wire_style(WireStyle::ChatCompletions)` 接入

### 交互式 TUI

`ui/` 目录包含一个 [Ink](https://github.com/vadimdemedes/ink)（React for CLI）编写的 TUI，驱动同一个 `mini-harness serve --stdio` 后端：

```bash
cd ui && npm install

make tui          # 接入 DeepSeek
# 或直接运行：npx tsx src/main.tsx --provider deepseek --workspace .. \
#             [--provider mock --workspace /tmp --event-log /tmp/tui.jsonl]
```

保留审批、由使用者决定是否执行时，工具触发审批在 TUI 里交互确认后才会执行。

### 配置

```toml
[model]
provider = "mock"          # 或 "openai" / "deepseek"
name = "deepseek-flash"

[session]
max_steps = 20
max_turn_time_ms = 600000
max_batch_tool_calls = 16
max_queued_inputs = 16

[execution]
max_output_bytes = 200000
default_timeout_ms = 120000
max_concurrent_processes = 8

[durable]
root = "~/.mini-harness"
flush = "buffered"         # 或 "synced"
checkpoint_every_events = 50

[permissions]
read_workspace = "allow"
edit_workspace = "ask"
bash = "ask"
```

时长类限制以零值表示"关闭"；计数类限制零值即立即拒绝。

## 语义要点

- **跨进程取消**：`cancel` 向事件日志写入终态事件；被取消进程在下一次追加前发现日志已变，以 durable 错误停止——宁可失败也不覆盖别人的写入
- **可观测性**：诊断日志只写 stderr（stdout 由协议独占），默认 `warn`，`RUST_LOG=mini_harness=debug` 查看事件持久化、工具执行、进程与 provider 尝试
- **审批流**：策略 `ask` 的工具调用持久化挂起，`approval.respond` 决定后续；策略拒绝记录独立的 `tool.policy_denied` 事件，与用户拒绝严格区分

## 测试

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

覆盖面：provider 行为契约（mock 与真实 provider 跑同一套六场景）、重放确定性 property 测试（种子化随机脚本）、跨进程追加/取消/修复探针、审批与队列的全相位边缘、进程组清理与输出上限回归。

## 状态与路线

核心阶段（类型与事件 → 事件溯源 → mock provider → 工具与执行器 → agent loop → 进程与取消 → 恢复 → provider → CLI 与协议 → scheduler）已完成；下一个深度专题是 OS 级 sandbox（macOS Seatbelt / Linux Landlock，fail-closed）。

## License

[MIT](LICENSE)
