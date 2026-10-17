# mini-harness 简化实施指南

**目标**: 将代码量从 17.7k 行降低到 ~8k 行，降低 55% 的理解成本  
**原则**: 保留教学核心价值，删除生产级复杂度  
**时间**: 约 1-2 周完成

---

## 实施路线图

```
Phase 1 (3-4 天)          Phase 2 (3-4 天)          Phase 3 (2-3 天)
━━━━━━━━━━━━━━━          ━━━━━━━━━━━━━━━          ━━━━━━━━━━━━━
删除 Checkpoint 系统      独立协议服务器             文档重写
删除 Session Actor        独立 Benchmark             代码注释增强
删除 Durable Queue        更新测试                   教学示例
简化 Tool Policy                                     
更新 CLI 命令                                        
```

---

## Phase 1: 核心简化（3-4 天）

### 任务 1.1: 删除 Checkpoint 系统

**删除文件** (-1200 行):
```bash
rm src/durable/checkpoint.rs
rm src/durable/recovery.rs
```

**修改文件**:

#### `src/durable/mod.rs`
```rust
// 删除
pub mod checkpoint;
pub mod recovery;
pub use checkpoint::Checkpoint;
pub use recovery::{recover_session, RecoveryOutcome};
```

#### `src/runtime/agent_loop.rs`
删除所有 checkpoint 相关代码：
```rust
// 删除这些函数
pub async fn create_checkpoint(...) { ... }
pub async fn restore_with_checkpoint(...) { ... }

// 只保留
pub async fn restore(...) {
    // 从事件日志完整重放
    let events = store.read_all().await?;
    let state = events.into_iter().fold(SessionState::new(session_id), |s, e| {
        reducer::apply(s, e)
    });
    // ...
}
```

#### `src/runtime/session.rs`
```rust
// 删除
pub async fn create_checkpoint(&mut self, path: impl AsRef<Path>) { ... }
pub async fn resume_with_policy_and_checkpoint(...) { ... }

// SessionCommand 枚举中删除
Checkpoint { path: PathBuf, reply: ... },
```

#### `src/runtime/session_actor.rs`
删除 `SessionCommand::Checkpoint` 分支：
```rust
// 在 run_actor 函数的 select! 中删除对应分支
```

#### `src/main.rs`
删除所有 checkpoint 相关 CLI 命令：
```rust
// 删除
Commands::Recover { ... } => { ... }
Commands::AbandonTurn { ... } => { ... }

// inspect 命令简化为只显示当前状态
Commands::Inspect { session_id } => {
    let store = create_store(&session_id)?;
    let state = replay_from_store(&store).await?;
    println!("{:#?}", state);
}
```

**更新测试**:
```bash
# 删除 checkpoint 相关测试
rm tests/checkpoint_*.rs
rm tests/recovery_*.rs

# 更新其他测试中的 checkpoint 调用
# 搜索并替换所有 create_checkpoint 调用
```

**验证**:
```bash
cargo build
cargo test
```

---

### 任务 1.2: 删除 Session Actor

**目标**: 只保留同步的 `Session` 接口，删除异步 Actor 模型

**删除文件** (-1300 行):
```bash
rm src/runtime/session_actor.rs
```

**修改 `src/runtime/session.rs`**:

删除所有 Actor 相关代码：
```rust
// 删除
pub enum SessionCommand { ... }
pub struct SessionHandle { ... }
impl SessionHandle { ... }
pub fn spawn(...) -> (SessionHandle, JoinHandle<()>) { ... }
pub fn spawn_with_id(...) { ... }
pub fn spawn_with_policy(...) { ... }
// ... 所有 spawn 函数

// 只保留
pub struct Session<P, E, S> {
    loop_: AgentLoop<P, E, S>,
}

impl<P, E, S> Session<P, E, S> {
    pub fn new(...) -> Self { ... }
    pub async fn start_turn(&mut self, input: String) -> Result<(TurnId, String), HarnessError> { ... }
    pub async fn cancel_turn(&mut self, turn: TurnId) -> Result<(), HarnessError> { ... }
    pub fn state(&self) -> &SessionState { ... }
}
```

**简化 `Session` 接口**:
```rust
// 删除复杂的并发接口，只保留基础同步接口
impl<P, E, S> Session<P, E, S> {
    // ✅ 保留
    pub fn new(provider, executor, tools, store) -> Self
    pub async fn start_turn(&mut self, input: String) -> Result<(TurnId, String), HarnessError>
    pub async fn cancel_turn(&mut self, turn: TurnId) -> Result<(), HarnessError>
    pub fn state(&self) -> &SessionState
    
    // ❌ 删除
    pub async fn start_turn_with_status(...)  // 过于复杂
    pub async fn wait_turn(...)               // Actor 专用
    pub async fn queue_approval(...)          // Durable Queue 专用
}
```

**更新 `src/main.rs`**:

```rust
// 所有命令改为直接使用 Session
async fn cmd_run(args: RunArgs) -> Result<()> {
    let provider = create_provider(&args)?;
    let executor = create_executor()?;
    let tools = create_tools()?;
    let store = create_store(&args.session_id)?;
    
    // 直接创建 Session（不再 spawn Actor）
    let mut session = Session::new(provider, executor, tools, store);
    
    let (turn_id, output) = session.start_turn(args.input).await?;
    println!("Turn {}: ", turn_id, output);
    Ok(())
}

async fn cmd_resume(args: ResumeArgs) -> Result<()> {
    let mut session = Session::resume(
        provider, executor, tools, store, args.session_id
    ).await?;
    
    let (turn_id, output) = session.start_turn(args.input).await?;
    println!("Turn {}: {}", turn_id, output);
    Ok(())
}
```

**更新测试**:
```bash
# 搜索所有使用 spawn 的测试
rg "spawn_with" tests/

# 改为直接使用 Session
# 例如：
# let (handle, _) = spawn_with_policy(...);
# 改为：
# let mut session = Session::new_with_policy(...);
```

---

### 任务 1.3: 删除 Durable Queue

**目标**: Turn 运行期间直接拒绝新输入，不再持久化排队

**修改 `src/runtime/state.rs`**:
```rust
pub struct SessionState {
    pub session_id: SessionId,
    pub history: Vec<HistoryItem>,
    pub turns: HashMap<TurnId, TurnState>,
    pub active_turn: Option<TurnId>,
    pub provider_continuation: Option<ProviderContinuation>,
    // ❌ 删除
    // pub pending_inputs: VecDeque<(TurnId, String)>,
    // pub max_queued_inputs: usize,
}
```

**修改 `src/durable/event.rs`**:
```rust
pub enum EventPayload {
    // ... 其他事件
    
    // ❌ 删除
    // UserInputQueued { turn_id: TurnId, input: String },
    // QueueFull { rejected_input: String },
}
```

**修改 `src/durable/reducer.rs`**:
```rust
pub fn apply(mut state: SessionState, event: Event) -> SessionState {
    match event.payload {
        // ... 其他分支
        
        // ❌ 删除
        // EventPayload::UserInputQueued { turn_id, input } => {
        //     state.pending_inputs.push_back((turn_id, input));
        //     state
        // }
    }
}
```

**修改 `src/runtime/agent_loop.rs`**:
```rust
pub async fn run_turn(&mut self, input: String) -> Result<(TurnId, String), HarnessError> {
    // ✅ 添加检查
    if self.state.active_turn.is_some() {
        return Err(HarnessError::TurnInProgress(
            "Cannot start new turn while another is running".into()
        ));
    }
    
    // ... 原有逻辑
}
```

**修改 `src/error.rs`**:
```rust
#[derive(Debug, Error)]
pub enum HarnessError {
    // ... 其他错误
    
    // ✅ 添加
    #[error("turn in progress: {0}")]
    TurnInProgress(String),
    
    // ❌ 删除
    // #[error("queue full: {0}")]
    // QueueFull(String),
}
```

---

### 任务 1.4: 简化 Tool Policy

**目标**: 只保留 `allow` 和 `ask` 两种策略，统一所有拒绝事件

**修改 `src/policy/mod.rs`**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,    // 直接执行
    Ask,      // 请求用户确认
    // ❌ 删除 Deny（总是通过 Ask + 用户拒绝来实现）
}

pub trait ToolPolicy: Send + Sync {
    fn check(&self, tool_name: &str) -> PolicyDecision;
}

pub struct DefaultPolicy;
impl ToolPolicy for DefaultPolicy {
    fn check(&self, tool_name: &str) -> PolicyDecision {
        match tool_name {
            "read_workspace" => PolicyDecision::Allow,
            "edit_workspace" | "bash" => PolicyDecision::Ask,
            _ => PolicyDecision::Ask,  // 未知工具默认 Ask
        }
    }
}
```

**修改 `src/durable/event.rs`**:
```rust
pub enum EventPayload {
    // ... 其他事件
    
    ToolApprovalRequested {
        call_id: ToolCallId,
        tool_name: String,
        input: serde_json::Value,
    },
    ToolApprovalResponded {
        call_id: ToolCallId,
        approved: bool,
        // ✅ approved=false 统一表示拒绝（不区分 policy 拒绝和用户拒绝）
    },
    
    // ❌ 删除
    // ToolPolicyDenied { ... }
}
```

**修改 `src/runtime/agent_loop.rs`**:
```rust
// 在工具执行循环中
for call in calls {
    let decision = self.policy.check(&call.name);
    
    match decision {
        PolicyDecision::Allow => {
            // 直接执行
            self.execute_tool(call).await?;
        }
        PolicyDecision::Ask => {
            // 请求审批
            self.request_approval(call).await?;
            // 等待用户响应...
            // 如果 approved=false，记录为工具错误
        }
    }
}
```

---

### 任务 1.5: 合并工具事件

**目标**: 每个工具调用从 5 个事件减少到 2 个

**修改 `src/durable/event.rs`**:
```rust
pub enum EventPayload {
    // ✅ 合并后的事件
    ToolRequestedAndApproved {
        call_id: ToolCallId,
        tool_name: String,
        input: serde_json::Value,
        // 如果是 Ask 策略，包含用户决策
        user_approved: Option<bool>,
    },
    ToolCompleted {
        call_id: ToolCallId,
        result: ToolResult,  // 包含输出或错误
        duration_ms: u64,
    },
    
    // ❌ 删除
    // ToolRequested { ... }
    // ToolApprovalRequested { ... }
    // ToolApprovalResponded { ... }
    // ToolStarted { ... }
    // ToolFinished { ... }
}
```

**修改 `src/runtime/agent_loop.rs`**:
```rust
async fn execute_tool_with_approval(&mut self, call: ToolCall) -> Result<ToolResult, HarnessError> {
    let decision = self.policy.check(&call.name);
    
    let user_approved = match decision {
        PolicyDecision::Allow => None,
        PolicyDecision::Ask => {
            // 等待用户审批（通过某种机制，例如回调）
            let approved = self.wait_for_user_approval(&call).await?;
            Some(approved)
        }
    };
    
    // ✅ 单个事件记录请求+审批
    self.append(
        Some(self.state.active_turn.unwrap()),
        EventPayload::ToolRequestedAndApproved {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            input: call.input.clone(),
            user_approved,
        }
    ).await?;
    
    // 如果用户拒绝，直接返回错误
    if user_approved == Some(false) {
        return Err(HarnessError::UserDenied("tool rejected by user".into()));
    }
    
    // 执行工具
    let start = Instant::now();
    let result = self.executor.execute(&call).await;
    let duration_ms = start.elapsed().as_millis() as u64;
    
    // ✅ 单个事件记录结果
    self.append(
        Some(self.state.active_turn.unwrap()),
        EventPayload::ToolCompleted {
            call_id: call.id,
            result: result.clone(),
            duration_ms,
        }
    ).await?;
    
    result
}
```

**更新 Reducer**:
```rust
pub fn apply(mut state: SessionState, event: Event) -> SessionState {
    match event.payload {
        EventPayload::ToolRequestedAndApproved { call_id, tool_name, user_approved, .. } => {
            if let Some(turn_id) = state.active_turn {
                if let Some(turn) = state.turns.get_mut(&turn_id) {
                    turn.pending_tools.insert(call_id.clone());
                    // 记录到 history（如果需要）
                }
            }
            state
        }
        EventPayload::ToolCompleted { call_id, result, .. } => {
            if let Some(turn_id) = state.active_turn {
                if let Some(turn) = state.turns.get_mut(&turn_id) {
                    turn.pending_tools.remove(&call_id);
                    turn.completed_tools.push((call_id, result));
                }
            }
            state
        }
        // ... 其他事件
    }
}
```

---

### 任务 1.6: 简化 CLI 命令

**修改 `src/main.rs`**:

```rust
#[derive(Parser)]
enum Commands {
    // ✅ 保留
    Run {
        input: String,
        #[arg(long)] provider: Option<String>,
        #[arg(long)] workspace: Option<PathBuf>,
    },
    Resume {
        session_id: String,
        input: String,
    },
    Cancel {
        session_id: String,
        turn_id: String,
    },
    Events {
        session_id: String,
    },
    Inspect {
        session_id: String,
    },
    Sessions,
    Demo {
        #[command(subcommand)]
        demo: DemoCommands,
    },
    Config,
    Doctor,
    
    // ❌ 删除
    // Serve { ... }        // 移到独立 crate
    // Benchmark { ... }    // 移到独立 crate
    // Tui { ... }          // 移到独立 crate
    // Recover { ... }      // checkpoint 功能删除
    // AbandonTurn { ... }  // recovery 功能删除
}
```

**简化后的命令说明**:
```bash
# 基础命令
cargo run -- run "hello"                      # 新会话
cargo run -- resume <session-id> "follow-up"  # 恢复会话
cargo run -- cancel <session-id> <turn-id>    # 取消 turn

# 查看命令
cargo run -- events <session-id>              # 查看事件日志
cargo run -- inspect <session-id>             # 查看当前状态
cargo run -- sessions                         # 列出所有会话

# 工具命令
cargo run -- config                           # 显示配置
cargo run -- doctor                           # 环境检查
cargo run -- demo read README.md              # 离线演示
```

---

### 任务 1.7: 更新测试

**删除不再需要的测试**:
```bash
rm tests/checkpoint_*.rs
rm tests/recovery_*.rs
rm tests/durable_queue.rs
rm tests/cross_process_cancel.rs  # 简化版不支持跨进程
```

**更新现有测试**:
```bash
# 搜索所有使用 spawn 的测试
rg "spawn" tests/ -l

# 批量替换模式
# 之前: let (handle, _) = spawn_with_policy(...);
# 之后: let mut session = Session::new_with_policy(...);

# 之前: handle.start_turn(input).await?;
# 之后: session.start_turn(input).await?;
```

**添加简化版测试**:
```rust
// tests/simple_session.rs
#[tokio::test]
async fn test_simple_turn() {
    let provider = Arc::new(MockProvider::new());
    let executor = Arc::new(LocalExecutor::new());
    let tools = Arc::new(ToolRegistry::new());
    let store = Arc::new(MemoryEventStore::new());
    
    let mut session = Session::new(provider, executor, tools, store);
    
    let (turn_id, output) = session.start_turn("hello".into()).await.unwrap();
    assert!(!output.is_empty());
}

#[tokio::test]
async fn test_turn_in_progress_error() {
    // ... 验证运行中不能启动新 turn
}

#[tokio::test]
async fn test_resume_from_events() {
    // ... 验证事件重放恢复
}
```

**验证所有测试通过**:
```bash
cargo test
```

---

## Phase 2: 独立模块（3-4 天）

### 任务 2.1: 独立协议服务器

**创建新 crate**:
```bash
mkdir -p mini-harness-server
cd mini-harness-server
cargo init --lib

# Cargo.toml
[package]
name = "mini-harness-server"
version = "0.1.0"
edition = "2024"

[dependencies]
mini-harness = { path = "../" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

**移动文件**:
```bash
# 从主项目移动到 mini-harness-server/src/
mv src/protocol/jsonl.rs mini-harness-server/src/jsonl.rs
mv src/protocol/jsonl_transport.rs mini-harness-server/src/transport.rs
mv src/protocol/event_projection.rs mini-harness-server/src/projection.rs

# 删除主项目的 protocol 目录
rm -rf src/protocol/
```

**mini-harness-server/src/lib.rs**:
```rust
mod jsonl;
mod transport;
mod projection;

pub use jsonl::{serve_stdio, SessionCommand};
pub use projection::project_event;
```

**创建独立二进制**:
```bash
# mini-harness-server/src/main.rs
use mini_harness_server::serve_stdio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve_stdio().await
}
```

**从主项目删除**:
```rust
// src/main.rs 中删除
Commands::Serve { ... } => { ... }
```

**README 更新**:
```markdown
## 协议服务器（可选）

如果需要 JSONL 协议服务器（用于 TUI 或自定义客户端），安装独立的 `mini-harness-server`:

​```bash
cd mini-harness-server
cargo install --path .
mini-harness-server --stdio
​```
```

---

### 任务 2.2: 独立 Benchmark

**创建新 crate**:
```bash
mkdir -p mini-harness-benchmark
cd mini-harness-benchmark
cargo init

# Cargo.toml
[package]
name = "mini-harness-benchmark"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "mini-harness-bench"
path = "src/main.rs"

[dependencies]
mini-harness = { path = "../" }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
```

**移动文件**:
```bash
mv src/benchmark.rs mini-harness-benchmark/src/main.rs
mv benchmark/ mini-harness-benchmark/tasks/

# 删除主项目的 benchmark 代码
# src/main.rs 中删除 Commands::Benchmark
```

**mini-harness-benchmark/README.md**:
```markdown
# mini-harness-benchmark

独立的 Benchmark 工具，用于评测 Agent Harness 在标准任务上的表现。

## 使用

​```bash
cargo run -- tasks/tasks.example.toml \
  --provider deepseek \
  --model deepseek-chat \
  --results ./results.json
​```

## Harbor 集成

参见 `harbor/README.md` 了解如何在容器化环境中运行 benchmark。
```

---

### 任务 2.3: 更新主项目结构

**src/lib.rs** (简化后):
```rust
pub mod config;
pub mod durable;
pub mod error;
pub mod executor;
pub mod model;
pub mod policy;
pub mod runtime;
pub mod tools;

// ❌ 删除
// pub mod protocol;
// pub mod scheduler;
```

**Cargo.toml** (简化后):
```toml
[package]
name = "mini-harness"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"

[[bin]]
name = "mini-harness"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
clap = { version = "4", features = ["derive"] }
toml = "0.8"

[dev-dependencies]
proptest = "1"
tempfile = "3"

[workspace]
members = [
    ".",
    "mini-harness-server",
    "mini-harness-benchmark",
]
```

---

## Phase 3: 文档优化（2-3 天）

### 任务 3.1: 重写 README.md

```markdown
# mini-harness

**从零实现的 Agent Harness 学习项目**

理解 Agent Harness 的核心概念：事件溯源、工具执行、崩溃恢复。

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
└── tools/        工具注册（read.rs, edit.rs, bash.rs）
```

## 快速开始

### 安装

```bash
git clone https://github.com/nothiny/mini-harness.git
cd mini-harness
cargo test  # 全部测试离线运行
```

### 离线演示

```bash
# 观察一次完整的工具调用链（不需要 API key）
cargo run -- demo read README.md

# 查看生成的事件日志
cat /tmp/demo-session.jsonl
```

### 接入真实模型

```bash
export DEEPSEEK_API_KEY=sk-...
cargo run -- run "总结这个项目" --provider deepseek --workspace .
```

## 学习路径

### 第一步：理解事件溯源（2 小时）

1. 阅读 `src/durable/event.rs` - 理解事件类型
2. 阅读 `src/durable/reducer.rs` - 理解状态重建
3. 运行 `demo read README.md` 并查看 JSONL 日志
4. 手动修改日志，重新 `inspect` 观察状态变化

### 第二步：理解 Agent Loop（2 小时）

1. 阅读 `src/runtime/agent_loop.rs` 的 `continue_turn` 函数
2. 理解循环：sample → tool calls → results → next step
3. 修改 `MockProvider` 实现自定义模型行为
4. 添加自定义工具

### 第三步：理解进程管理（2 小时）

1. 阅读 `src/executor/process.rs` - 进程组、SIGKILL
2. 阅读 `src/executor/local.rs` - 输出限制
3. 实验：启动长时间任务并 cancel

### 第四步：理解崩溃恢复（1 小时）

1. 启动一个 turn，中途 `kill -9` 强制终止
2. 使用 `resume` 恢复会话
3. 观察事件重放过程

**总学习时间**: 7-8 小时

## 进阶功能

### 协议服务器（可选）

如果需要构建自定义 UI：

```bash
cd mini-harness-server
cargo run -- --stdio
```

参见 `mini-harness-server/README.md`

### Benchmark（可选）

如果需要评测性能：

```bash
cd mini-harness-benchmark
cargo run -- tasks/example.toml --provider deepseek
```

参见 `mini-harness-benchmark/README.md`

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
| 代码量 | ~8k 行 | 50k+ 行 |
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

### Q: 可以简化为不记录事件吗？

不推荐。不记录事件意味着：
- 崩溃后无法知道"工具是否已执行"
- 可能导致重复执行（例如两次删除同一文件）
- 违背 Event Sourcing 原则

### Q: 为什么不支持工具并行？

简化版串行执行，便于理解。生产级需要并行执行 + 依赖图管理。

### Q: 如何添加自定义工具？

参见 `examples/custom_tool.rs`（TODO: 添加示例）

## 下一步

1. 完成学习路径的四个步骤
2. 阅读 `ARCHITECTURE.md` 了解设计决策
3. 参考 `SIMPLIFICATION_PROPOSAL.md` 了解简化过程
4. 对比 Claude Code 等生产系统的实现

## License

MIT
```

---

### 任务 3.2: 添加代码注释

**src/durable/reducer.rs** 增强注释:
```rust
/// 确定性 Reducer: 从事件流重建完整状态
///
/// **核心不变量**: 相同的事件序列 → 相同的状态（无论重放多少次）
///
/// 这是 Event Sourcing 的核心：状态是事件的"折叠"(fold)。
///
/// # 为什么需要确定性？
///
/// 1. **崩溃恢复**: 进程崩溃后，从事件日志重放即可恢复到崩溃前状态
/// 2. **时间旅行**: 可以重放到任意历史时刻，用于调试
/// 3. **一致性**: 多个进程读取同一事件日志，得到相同的状态视图
///
/// # 如何保证确定性？
///
/// - ❌ 不使用 `Instant::now()`（时间是外部输入）
/// - ❌ 不使用 `rand::random()`（随机数是不可重现的）
/// - ✅ 所有时间戳来自事件本身的 `timestamp` 字段
/// - ✅ 所有随机性来自模型响应（已记录在事件中）
pub fn apply(mut state: SessionState, event: Event) -> SessionState {
    match event.payload {
        EventPayload::TurnStarted { turn_id } => {
            // 创建新 Turn 状态
            state.active_turn = Some(turn_id);
            state.turns.insert(turn_id, TurnState::new());
            state
        }
        // ... 其他事件
    }
}
```

**src/runtime/agent_loop.rs** 增强注释:
```rust
/// Agent Loop: 模型-工具循环的核心实现
///
/// # 执行流程
///
/// ```text
/// ┌─────────────────────────────────────┐
/// │  1. Sample 模型                      │
/// │     ↓                               │
/// │  2. 解析响应                         │
/// │     ├─→ 文本回答 → 结束 Turn        │
/// │     └─→ 工具调用 → 继续             │
/// │           ↓                         │
/// │  3. 执行工具（带审批）               │
/// │     ↓                               │
/// │  4. 追加结果到 context              │
/// │     ↓                               │
/// │  5. 返回步骤 1（直到达到 max_steps） │
/// └─────────────────────────────────────┘
/// ```
///
/// # 为什么需要步数限制？
///
/// 防止模型陷入无限循环（例如反复调用同一工具）。
///
/// # 为什么需要工具审批？
///
/// 防止模型执行危险操作（例如 `bash rm -rf /`）。
pub async fn continue_turn(&mut self, turn_id: TurnId) -> Result<String, HarnessError> {
    let mut step = 0;
    
    loop {
        // 检查步数限制
        if step >= self.config.max_steps {
            return Err(HarnessError::MaxStepsReached);
        }
        
        // 采样模型
        let response = self.sample_model().await?;
        
        match response {
            Response::Text(text) => {
                // 模型返回文本 → Turn 结束
                return Ok(text);
            }
            Response::ToolCalls(calls) => {
                // 模型请求工具 → 执行并继续
                self.execute_tools(calls).await?;
                step += 1;
            }
        }
    }
}
```

**src/executor/process.rs** 增强注释:
```rust
/// 进程组级取消：清理整个进程树
///
/// # 问题
///
/// 直接 `kill(pid)` 只能杀死父进程，子进程会变成孤儿进程：
///
/// ```text
/// bash (pid=100)
///   └─ sleep 1000 (pid=101)  ← kill(100) 后变成孤儿
/// ```
///
/// # 解决方案
///
/// 创建新进程组，使用 `kill(-pgid, SIGKILL)` 杀死整组：
///
/// ```text
/// bash (pid=100, pgid=100)
///   └─ sleep 1000 (pid=101, pgid=100)
/// 
/// kill(-100, SIGKILL) → 两个进程都被杀死
/// ```
///
/// # 跨平台差异
///
/// - Unix: `kill(-pid, SIGKILL)`
/// - Windows: `taskkill /T /F /PID <pid>`
#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);  // 0 = 创建新进程组
}
```

---

### 任务 3.3: 创建教学示例

**examples/01_event_sourcing.rs**:
```rust
//! 示例 1: 理解事件溯源
//!
//! 运行: cargo run --example 01_event_sourcing

use mini_harness::durable::{Event, EventPayload, reducer, SessionState};
use mini_harness::runtime::ids::{SessionId, TurnId};

fn main() {
    let session_id = SessionId::new();
    
    // 模拟一系列事件
    let events = vec![
        Event::new(EventPayload::TurnStarted { turn_id: TurnId::new() }),
        Event::new(EventPayload::UserInputRecorded { input: "hello".into() }),
        Event::new(EventPayload::ModelResponseRecorded { text: "Hi!".into() }),
        Event::new(EventPayload::TurnCompleted { turn_id: TurnId::new() }),
    ];
    
    // 从事件重建状态
    let state = events.into_iter().fold(
        SessionState::new(session_id),
        |state, event| reducer::apply(state, event)
    );
    
    println!("重建后的状态:");
    println!("  历史记录数: {}", state.history.len());
    println!("  活跃 Turn: {:?}", state.active_turn);
    
    println!("\n关键点:");
    println!("  1. 状态完全由事件决定");
    println!("  2. 重放两次得到相同状态");
    println!("  3. 不依赖外部输入（时间、随机数等）");
}
```

**examples/02_agent_loop.rs**:
```rust
//! 示例 2: 理解 Agent Loop
//!
//! 运行: cargo run --example 02_agent_loop

use mini_harness::runtime::Session;
use mini_harness::model::MockProvider;
use mini_harness::executor::LocalExecutor;
use mini_harness::tools::ToolRegistry;
use mini_harness::durable::MemoryEventStore;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let provider = Arc::new(MockProvider::new());
    let executor = Arc::new(LocalExecutor::new());
    let tools = Arc::new(ToolRegistry::new());
    let store = Arc::new(MemoryEventStore::new());
    
    let mut session = Session::new(provider, executor, tools, store);
    
    println!("开始 Turn...");
    let (turn_id, output) = session.start_turn("hello".into()).await.unwrap();
    
    println!("\nTurn {} 完成:", turn_id);
    println!("  输出: {}", output);
    
    println!("\n查看生成的事件:");
    for event in session.state().turns[&turn_id].events.iter() {
        println!("  - {:?}", event.payload);
    }
}
```

**examples/03_tool_execution.rs**:
```rust
//! 示例 3: 理解工具执行
//!
//! 运行: cargo run --example 03_tool_execution

// ... 演示工具调用、审批、取消
```

**examples/04_crash_recovery.rs**:
```rust
//! 示例 4: 理解崩溃恢复
//!
//! 运行: cargo run --example 04_crash_recovery

// ... 演示事件重放恢复
```

---

## 验证清单

### Phase 1 完成后

- [ ] `cargo build` 成功
- [ ] `cargo test` 全部通过
- [ ] `cargo run -- demo read README.md` 工作正常
- [ ] `cargo run -- run "hello"` 工作正常
- [ ] 代码量减少到 ~8k-10k 行
- [ ] 事件类型减少到 15-20 个
- [ ] CLI 命令减少到 7 个核心命令

### Phase 2 完成后

- [ ] `mini-harness-server` 独立编译
- [ ] `mini-harness-benchmark` 独立编译
- [ ] 主项目不依赖这两个 crate
- [ ] Workspace 结构正确
- [ ] 文档更新完成

### Phase 3 完成后

- [ ] README.md 包含 5 分钟快速理解
- [ ] README.md 包含学习路径
- [ ] 核心代码有详细注释
- [ ] 提供 4 个教学示例
- [ ] 测试覆盖核心功能

---

## 预期成果

### 简化前

```
代码量: 17.7k 行
文件数: 45 个
核心概念: 12 个
学习时间: 20+ 小时
CLI 命令: 15 个
```

### 简化后

```
代码量: ~8k 行（-55%）
文件数: ~25 个（-44%）
核心概念: 5 个（-58%）
学习时间: 7-8 小时（-60%）
CLI 命令: 7 个（-53%）
```

### 保留的核心价值

✅ Event Sourcing 完整实现  
✅ Agent Loop 核心逻辑  
✅ 进程管理与取消  
✅ 崩溃恢复能力  
✅ 工具审批流程  
✅ 真实模型接入  
✅ 完整测试覆盖

### 可选扩展（独立 crate）

📦 JSONL 协议服务器  
📦 Benchmark 系统  
📦 TUI 界面

---

## 常见问题

### Q: 删除 Checkpoint 后如何快速恢复？

A: 简化版只支持"从头重放"。对于学习项目，会话不会太长，重放速度足够快。

生产环境需要 Checkpoint 时，可以参考 git history 中的实现。

### Q: 删除 Actor 后如何支持并发？

A: 简化版是单线程同步模型。如果需要并发（例如同时运行多个会话），可以：
1. 在外层用 `tokio::spawn` 启动多个 Session
2. 每个 Session 独立运行，通过事件日志隔离

生产环境需要 Actor 时，可以参考 git history 中的实现。

### Q: 为什么不支持 Durable Queue？

A: 简化版认为"一次只运行一个 Turn"对学习足够。

生产环境需要队列时（例如 Web 服务），可以在 Session 外层实现队列。

---

## 下一步

1. 选择一个起点（建议从 Phase 1.1 开始）
2. 逐步实施，每完成一个任务就 `cargo test` 验证
3. 遇到问题时参考 git history 中的原始实现
4. 完成后重新审视架构，看是否达到"初学者友好"的目标
