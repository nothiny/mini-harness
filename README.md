# mini-harness

**从零实现的 Agent Harness 学习项目**

理解 Agent Harness 的核心概念：事件溯源、工具执行、崩溃恢复。

harness 是模型循环之外的全部系统——工具执行与授权、进程管理、事件日志、崩溃恢复。本项目以 ~10k 行 Rust 把这些边界砌了出来（协议服务器与 benchmark 另有独立 crate），并用一组 160+ 无网络依赖的测试钉住核心语义。

## 5 分钟快速理解

### 核心概念

1. **事件溯源**: 所有状态变化都记录为事件，状态 = reduce(事件列表)
2. **Agent Loop**: 模型采样 → 工具调用 → 返回结果 → 下一轮
3. **工具执行**: 安全地执行子进程，限制输出，支持取消
4. **崩溃恢复**: 从事件日志重放，恢复到崩溃前的状态
5. **审批流程**: 危险工具需要用户确认才能执行

### 代码结构

```
src/
├── durable/      事件溯源（event.rs, reducer.rs, event_log.rs）
├── runtime/      Agent 循环（agent_loop.rs, session.rs）
├── executor/     进程执行（local.rs, process.rs）
├── model/        模型接口（provider.rs, mock.rs, openai.rs）
├── policy/       工具审批（allow / ask）
└── tools/        工具注册（read.rs, edit.rs, bash.rs）

mini-harness-server/     JSONL 协议服务器（可选，供 TUI/自定义客户端）
mini-harness-benchmark/  Benchmark 运行器（可选）
ui/                      Ink TUI（TypeScript，经协议服务器驱动）
```

## 快速开始

### 安装

```bash
git clone https://github.com/nothiny/mini-harness.git
cd mini-harness
cargo test --workspace  # 全部测试离线运行
```

### 离线演示

```bash
# 观察一次完整的工具调用链（不需要 API key）
cargo run -- demo read README.md

# 查看生成的事件流（每行一个事实）
cargo run -- demo read README.md --json
```

### 接入真实模型

```bash
export DEEPSEEK_API_KEY=sk-...
cargo run -- run "总结这个项目" --provider deepseek --workspace .
```

## CLI 命令

```bash
# 基础命令
cargo run -- run "hello"                      # 新会话
cargo run -- resume <session-id> "follow-up"  # 恢复会话
cargo run -- cancel <session-id> <turn-id>    # 取消 turn

# 查看命令
cargo run -- events <session-id>              # 查看事件日志
cargo run -- inspect <session-id>             # 重放并查看状态
cargo run -- sessions                         # 列出所有会话

# 工具命令
cargo run -- config                           # 显示配置
cargo run -- doctor                           # 环境检查
cargo run -- demo read README.md              # 离线演示
```

## 学习路径

### 第一步：理解事件溯源（2 小时）

1. 阅读 `src/durable/event.rs` - 理解事件类型
2. 阅读 `src/durable/reducer.rs` - 理解状态重建
3. 运行 `cargo run --example 01_event_sourcing`
4. 运行 `demo read README.md --json` 并对照 JSONL 日志
5. 手动修改日志，重新 `inspect` 观察状态变化

### 第二步：理解 Agent Loop（2 小时）

1. 阅读 `src/runtime/agent_loop.rs` 的 `continue_turn` 函数
2. 理解循环：sample → tool calls → results → next step
3. 运行 `cargo run --example 02_agent_loop`
4. 修改 `MockProvider` 实现自定义模型行为
5. 添加自定义工具

### 第三步：理解进程管理（2 小时）

1. 阅读 `src/executor/process.rs` - 进程组、SIGKILL
2. 阅读 `src/executor/local.rs` - 输出限制
3. 运行 `cargo run --example 03_tool_execution`
4. 实验：启动长时间任务并 cancel

### 第四步：理解崩溃恢复（1 小时）

1. 启动一个 turn，中途 `kill -9` 强制终止
2. 使用 `resume` 恢复会话
3. 观察事件重放过程
4. 运行 `cargo run --example 04_crash_recovery`

**总学习时间**: 7-8 小时

## 进阶功能

### 协议服务器（可选）

如果需要构建自定义 UI（例如 Ink TUI）：

```bash
cargo build -p mini-harness-server
./target/debug/mini-harness-server --stdio
```

参见 `mini-harness-server/`。

### Benchmark（可选）

如果需要评测性能：

```bash
cargo run -p mini-harness-benchmark -- tasks/tasks.example.toml --provider deepseek
```

参见 `mini-harness-benchmark/`。

## 架构设计

### 为什么用事件溯源？

传统做法：直接修改内存状态
- ❌ 崩溃后无法恢复
- ❌ 无法追溯历史决策
- ❌ 难以调试

事件溯源做法：所有变化都是事件
- ✅ 崩溃后重放事件即可恢复
- ✅ 完整的决策历史
- ✅ 时间旅行调试

### 为什么需要进程组？

传统做法：直接 `kill(pid)`
- ❌ 子进程会泄漏（例如 bash 启动的子进程）
- ❌ 无法清理孙进程

进程组做法：`kill(-pgid)`
- ✅ 一次性清理整个进程树
- ✅ 防止僵尸进程

### 为什么需要工具审批？

传统做法：模型直接执行
- ❌ 模型可能执行危险命令（`rm -rf /`）
- ❌ 用户无法干预

审批做法：危险工具需确认
- ✅ 用户可以拒绝危险操作
- ✅ 审批决策记录在事件中

## 与生产系统的差异

mini-harness 是**学习项目**，与生产级 Harness 的差异：

| 特性 | mini-harness | 生产级 (例如 Claude Code) |
|------|--------------|---------------------------|
| 代码量 | ~10k 行 | 50k+ 行 |
| Checkpoint | ❌ 只有重放 | ✅ 增量快照 |
| 并发模型 | 同步 Session | Actor/Channel 并发 |
| OS 沙箱 | ❌ 只有路径检查 | ✅ Landlock/Seatbelt |
| Context 管理 | 简单 sliding window | 智能压缩 + 摘要 |
| 工具并行 | 串行执行 | 并行执行 |
| Streaming | ❌ | ✅ |

**学习建议**: 先理解 mini-harness 的核心概念，再研究生产系统的优化。

## 常见问题

### Q: 为什么事件这么多？

每个工具调用记录 2 个事件（请求+完成），确保崩溃后可以恢复。
请求事件同时携带审批标记（`requires_approval`），用户决定单独记录。

### Q: 可以简化为不记录事件吗？

不推荐。不记录事件意味着：
- 崩溃后无法知道"工具是否已执行"
- 可能导致重复执行（例如两次删除同一文件）
- 违背 Event Sourcing 原则

### Q: 为什么不支持工具并行？

简化版串行执行，便于理解。生产级需要并行执行 + 依赖图管理。

### Q: 如何添加自定义工具？

实现 `tools/spec.rs` 中的 `Tool` trait 并注册到 `ToolRegistry`，
参考 `src/tools/read.rs` 的结构。

## License

MIT
