# mini-harness 简历文案 - 简化版本 + 核心理解

---

## 简历文案（最终版，基于简化后的架构）

### mini-harness | Agent Harness 核心运行时 | Rust

**项目定位**: 从零实现 Agent Harness 核心能力，深入理解"模型循环之外的全部系统"

#### 核心架构（~8k 行，简化后）

**Event Sourcing 状态管理**
- JSONL 事件日志作为唯一事实来源，确定性 Reducer 从事件重建状态（重放两次必然同态）
- 15+ 核心事件类型覆盖完整生命周期：Session、Turn、Tool、Model Response
- Property-Based Testing 验证重放确定性（24 种随机场景），支持时间旅行调试和崩溃恢复

**Agent Loop 执行引擎**
- 实现 sample → tool calls → results → next step 的核心循环，支持 max_steps 限制和 turn-level 超时
- Context 管理：sliding window 策略（最近 512KB history），provider cursor 避免重复发送缓存响应
- 中途取消机制：通过 durable event 通知运行中的 turn，保证取消决策持久化

**工具执行与边界控制**
- 三级授权策略（allow/ask/deny），危险工具（edit/bash）需用户审批后执行
- Workspace 边界强制：Linux `openat2(RESOLVE_BENEATH)`、Unix `openat(O_NOFOLLOW)`、路径规范化防逃逸
- 完整生命周期跟踪：每个工具调用记录 requested → completed/failed 事件，支持审计和回放

**进程生命周期管理**
- 进程组级取消：Unix `kill(-pgid, SIGKILL)` / Windows `taskkill /T` 清理整个进程树，防止僵尸进程
- 三层输出限制：per-tool limit、cumulative limit、UTF-8 边界安全截断，防止单个工具 OOM
- 全局并发控制：tokio Semaphore 限制同时运行的进程数，超限返回结构化错误而非无限排队

**Provider 抽象与多协议支持**
- ModelProvider trait 统一采样接口，支持 tool_choice 控制和 context cursor（避免重复缓存）
- 双协议支持：OpenAI Responses API 和 Chat Completions API 自动选择，兼容 DeepSeek/Kimi/Qwen
- MockProvider 实现确定性测试，支持完全离线开发（无需 API key）
- 契约测试：6 个核心场景保证所有 Provider 行为一致性

**工程质量与可扩展性**
- 65+ 单元测试 + Property-Based Testing（proptest 随机场景）+ 集成测试，100% 核心路径覆盖
- 模块化设计：Durable 层（持久化）、Runtime 层（循环逻辑）、Executor 层（进程管理）、Policy 层（授权）严格分层
- 完整文档：架构审查报告（识别 12 个问题 + 优先级改进方案）、简化实施指南（降低 55% 理解成本）

**技术栈**: Rust (2024 edition) · Tokio · Event Sourcing · JSONL · Property-Based Testing  
**代码量**: 简化前 17.7k 行 → 简化后 ~8k 行（保留核心教学价值）  
**GitHub**: [链接]

---

## 核心能力拆解（体现对 Harness 的理解）

### 1. 什么是 Agent Harness？

**定义**: Agent Harness = "模型循环之外的全部系统"

```
┌─────────────────────────────────────────────┐
│  Harness 边界                                │
│  ┌─────────────────────────────────────┐    │
│  │  Agent Loop                         │    │
│  │  ┌───────┐   ┌──────┐   ┌────────┐ │    │
│  │  │ Model │──▶│ Tool │──▶│ Result │ │    │
│  │  └───────┘   └──────┘   └────────┘ │    │
│  │      ▲          │            │      │    │
│  └──────┼──────────┼────────────┼──────┘    │
│         │          │            │           │
│    Provider    Executor     Reducer         │
│    (外部)      (进程管理)   (状态管理)        │
└─────────────────────────────────────────────┘
```

**Harness 的核心职责**:
1. **状态管理** - 会话状态、对话历史、工具执行记录如何持久化和恢复
2. **执行控制** - 工具如何安全执行、如何取消、如何防止资源耗尽
3. **循环编排** - 模型-工具循环如何推进、何时终止、如何处理错误
4. **外部集成** - 如何抽象不同模型 Provider、如何注册工具、如何授权

**我的项目验证**: 完整实现了这 4 个核心职责，用 8k 行代码展示 Harness 的最小完整实现。

---

### 2. 为什么选择 Event Sourcing？

**核心洞察**: Agent 系统的本质挑战是"**如何在不可靠环境中保证状态一致性**"

#### 传统方案的问题

```rust
// ❌ 传统方案：内存状态 + 定期持久化
struct Session {
    state: SessionState,  // 内存中
}

impl Session {
    async fn run_turn(&mut self) {
        self.state.active_turn = Some(turn_id);  // 内存修改
        // ... 执行工具
        self.save_checkpoint().await?;  // 定期保存
    }
}
```

**问题**:
1. 进程 kill -9 后，checkpoint 间的状态丢失
2. 无法追溯"为什么系统处于这个状态"
3. 多进程并发时需要复杂的锁机制

#### Event Sourcing 的解决

```rust
// ✅ Event Sourcing：事件日志 + 确定性 Reducer
struct Session {
    store: EventStore,   // JSONL 日志
    state: SessionState, // 由事件重建
}

impl Session {
    async fn run_turn(&mut self) {
        // 1. 先持久化事件
        self.store.append(TurnStarted { turn_id }).await?;
        
        // 2. 再更新内存状态
        self.state = reducer::apply(self.state, event);
    }
    
    // 崩溃恢复：重放所有事件
    async fn recover() -> Session {
        let events = store.read_all().await?;
        let state = events.fold(SessionState::new(), reducer::apply);
        Session { store, state }
    }
}
```

**优势**:
1. ✅ **完整可恢复**: kill -9 后从事件日志完整重放
2. ✅ **审计追溯**: 所有决策（工具调用、审批）都有记录
3. ✅ **时间旅行**: 可以重放到任意历史时刻调试
4. ✅ **多进程安全**: 只要追加顺序正确，状态一致

**实现要点**:
- **确定性 Reducer**: 不能依赖 `now()`/`random()`，时间戳来自事件本身
- **事件完整性**: 一个操作的所有副作用必须通过事件表达
- **验证机制**: Property-Based Testing 验证"重放两次 → 相同状态"

**我的项目验证**: 24 种随机场景验证重放确定性，所有状态变化都通过 15+ 事件类型记录。

---

### 3. 进程管理的核心挑战

**问题**: 如何安全地执行模型请求的 bash 命令？

#### 挑战 1: 子进程泄漏

```bash
# 模型调用 bash 工具
bash "nohup sleep 1000 &"

# 传统方案：kill(pid) 只杀 bash 父进程
kill 12345  # bash 进程

# 结果：sleep 1000 变成孤儿进程，泄漏！
```

**解决方案**: 进程组级取消

```rust
// 创建新进程组
#[cfg(unix)]
command.process_group(0);  // 0 = 创建新进程组

// 取消时杀死整个进程组
unsafe {
    libc::kill(-pid, libc::SIGKILL);  // 负号 = 进程组
}
```

**跨平台实现**:
- Unix: `setpgid(0, 0)` + `kill(-pgid, SIGKILL)`
- Windows: `CREATE_NEW_PROCESS_GROUP` + `taskkill /T /F`

#### 挑战 2: 输出爆内存

```bash
# 恶意/失控命令
bash "cat /dev/zero"  # 无限输出
bash "find / -type f -exec cat {} \;"  # 巨大输出
```

**解决方案**: 三层限制

```rust
pub struct OutputLimits {
    pub per_tool: usize,      // 每个工具最多 200KB
    pub cumulative: usize,    // 单个 turn 累计最多 2MB
    pub utf8_safe: bool,      // 不能截断 UTF-8 中间
}
```

**并发实现**:
```rust
// 多个工具并发执行时竞争 cumulative limit
let combined = Arc::new(AtomicUsize::new(0));

// 每个 reader 尝试分配配额
fn reserve_combined(counter: &AtomicUsize, limit: usize, requested: usize) -> usize {
    loop {
        let current = counter.load(Ordering::Relaxed);
        if current >= limit { return 0; }
        let allowed = requested.min(limit - current);
        if counter.compare_exchange_weak(current, current + allowed, ...).is_ok() {
            return allowed;
        }
    }
}
```

#### 挑战 3: UTF-8 安全截断

```
原始输出: "你好世界" (12 字节)
           E4 BD A0 E5 A5 BD E4 B8 96 E7 95 8C

如果 limit=10，不能直接截断：
错误: E4 BD A0 E5 A5 BD E4 B8 96 E7  (最后2字节是"界"的一半)
正确: E4 BD A0 E5 A5 BD              (6字节，"你好")
```

**实现**:
```rust
let valid_len = match std::str::from_utf8(&bytes) {
    Ok(_) => bytes.len(),
    Err(e) if e.error_len().is_none() => e.valid_up_to(),  // 截断点
    Err(_) => bytes.len(),  // 中间有非法字节，保留全部
};
```

**我的项目验证**: 集成测试覆盖所有边界 case（子进程树清理、输出限制、UTF-8 截断）。

---

### 4. 中途取消的正确语义

**场景**: 用户在 turn 运行期间点击"取消"

#### 错误的实现

```rust
// ❌ 直接 kill 进程
async fn cancel_turn(&mut self, turn_id: TurnId) {
    self.executor.kill_all();  // 副作用已发生
    self.state.active_turn = None;  // 内存修改
}
// 问题：进程崩溃后，不知道"取消"是否已生效
```

#### 正确的实现（Durable Cancel）

```rust
// ✅ 先持久化取消事件
async fn cancel_turn(&mut self, turn_id: TurnId) {
    // 1. 写入取消事件
    self.store.append(TurnCancelRequested { turn_id }).await?;
    
    // 2. 通知运行中的 executor
    self.executor.cancel();
    
    // 3. 等待 executor 确认
    // executor 下次追加事件前会发现日志已变，主动停止
}
```

**关键设计**:
1. **取消是一个事件**，不是内存标志位
2. **运行进程通过读日志发现取消**，而非共享内存 flag（避免竞态）
3. **Fail-Stop 语义**: 发现日志变化后拒绝追加，而非覆盖

**跨进程取消**:
```
进程 A（用户）        进程 B（运行 turn）
    │                      │
    │  cancel_turn()       │
    ├──▶ 写入 Cancel event │
    │                      │ 正在执行工具
    │                      ├──▶ 尝试追加 ToolCompleted
    │                      ├──▶ 检测到 last_seq 变化
    │                      └──▶ 读取日志，发现 Cancel
    │                          返回 Cancelled 错误
```

**我的项目验证**: 跨进程集成测试模拟进程 A cancel、进程 B 正在运行的竞态场景。

---

### 5. Context 管理的核心权衡

**问题**: 如何在有限的 context window 中保留最重要的信息？

#### 当前实现（简化版）

```rust
pub fn build_context(&self) -> Context {
    let mut history_bytes = 0;
    let mut items = Vec::new();
    
    // 从最近的记录开始向前取
    for item in self.state.history.iter().rev() {
        // 遇到 provider cursor 停止（避免重复发送缓存）
        if item.is_cursor() { break; }
        
        // 单条记录截断到 16KB
        let truncated = item.truncate(16 * 1024);
        history_bytes += truncated.len();
        
        // 累计达到 512KB 停止
        if history_bytes > 512 * 1024 { break; }
        
        items.push(truncated);
    }
    
    items.reverse();  // 恢复时间顺序
    Context { items }
}
```

**优点**: 简单、可预测、无需额外模型调用  
**缺点**: 不考虑内容重要性（初始 task 可能被丢弃）

#### 生产级需求

```
对话历史重要性分布：
┌────────────────────────────────────────────┐
│ 系统指令       ████████████  (最重要)        │
│ 初始任务       ███████████   (必须保留)      │
│ 工具定义       ████████      (schema)       │
│ 最近 3 轮对话  ██████████    (必须完整)      │
│ 中间工具输出   ███           (可压缩/舍弃)    │
│ 历史对话       ██            (可 summarize) │
└────────────────────────────────────────────┘
```

**改进方向**:
1. **分层管理**: System > Task > Recent > History
2. **智能压缩**: 历史对话用小模型 summarize
3. **Token 预算**: 全局 token budget，预留 response token
4. **Message 完整性**: 不能截断 tool call/result 对

**我的设计（如果追问）**:
```rust
pub struct ContextManager {
    // 永远保留
    system_instructions: String,
    initial_task: String,
    tool_schemas: Vec<Tool>,
    
    // 动态调整
    recent_turns: VecDeque<Turn>,    // 最近 N 轮完整保留
    history_summary: Option<String>, // 历史摘要
    
    // 预算控制
    token_budget: TokenBudget,
}
```

**我的项目验证**: 当前实现了简单 sliding window，架构审查中识别此为 P2 改进点。

---

### 6. Provider 抽象的关键设计

**问题**: 不同模型 API 差异巨大，如何统一？

#### 核心接口

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// 核心采样接口
    async fn sample(&self, request: SamplingRequest) -> Result<SamplingResponse>;
    
    /// 可选：支持 context cursor（避免重复发送缓存内容）
    fn supports_cursor(&self) -> bool { false }
}

pub struct SamplingRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub max_tokens: Option<usize>,
    pub tool_choice: ToolChoice,  // auto / required / specific tool
}

pub enum SamplingResponse {
    Text(String),
    ToolCalls(Vec<ToolCall>),
    Refusal(String),
}
```

**关键设计决策**:

1. **为什么用 enum 而非 struct？**
   - 模型响应是"互斥"的：要么文本，要么工具调用，要么拒绝
   - `enum` 强制调用者处理所有情况

2. **为什么有 `tool_choice`？**
   - 某些场景需要强制模型调用工具（例如结构化输出）
   - 某些场景禁止工具调用（例如 summarize 阶段）

3. **为什么有 `supports_cursor`？**
   - OpenAI Responses API 支持 cursor（避免重复发送缓存）
   - Chat Completions API 不支持（每次发送完整 history）

#### 多协议支持

```rust
pub enum WireStyle {
    Responses,         // OpenAI Responses API
    ChatCompletions,   // OpenAI Chat Completions API (DeepSeek 等兼容)
}

impl OpenAiProvider {
    pub fn new(config: OpenAiProviderConfig) -> Self {
        let wire_style = match config.provider_type {
            "openai" => WireStyle::Responses,
            "deepseek" | "kimi" | "qwen" => WireStyle::ChatCompletions,
            _ => WireStyle::ChatCompletions,  // 默认兼容协议
        };
        // ...
    }
}
```

#### 契约测试（保证一致性）

```rust
async fn test_provider_contract<P: ModelProvider>(provider: P) {
    // 场景 1: 纯文本响应
    let resp = provider.sample(text_request()).await?;
    assert!(matches!(resp, SamplingResponse::Text(_)));
    
    // 场景 2: 工具调用
    let resp = provider.sample(tool_request()).await?;
    assert!(matches!(resp, SamplingResponse::ToolCalls(_)));
    
    // 场景 3: 拒绝回答
    let resp = provider.sample(unsafe_request()).await?;
    assert!(matches!(resp, SamplingResponse::Refusal(_)));
    
    // 场景 4: 空输入
    let resp = provider.sample(empty_request()).await?;
    assert!(resp.is_err());
    
    // 场景 5: 并发请求
    let (r1, r2) = tokio::join!(
        provider.sample(req1),
        provider.sample(req2)
    );
    assert!(r1.is_ok() && r2.is_ok());
    
    // 场景 6: 错误处理
    let resp = provider.sample(invalid_request()).await;
    assert!(resp.is_err());
}

// MockProvider 和 OpenAiProvider 都必须通过这 6 个场景
```

**我的项目验证**: MockProvider 和 OpenAI/DeepSeek Provider 跑同一套 6 场景测试，保证行为一致性。

---

### 7. 测试策略的深度思考

**核心理念**: Agent 系统的复杂度在于"**状态 × 并发 × 外部依赖**"的组合爆炸

#### 测试金字塔

```
         △  集成测试 (10%)
        ╱ ╲  - 跨进程并发
       ╱   ╲  - 完整 Agent Loop
      ╱     ╲  - 崩溃恢复
     ╱       ╲
    ╱─────────╲ Property-Based Testing (20%)
   ╱           ╲  - 重放确定性
  ╱             ╲  - 事件完整性
 ╱               ╲
╱─────────────────╲ 单元测试 (70%)
                    - Reducer
                    - Process
                    - Tools
```

#### Property-Based Testing 的威力

```rust
// 传统单元测试：手写固定场景
#[test]
fn test_replay_simple() {
    let events = vec![TurnStarted, ToolCalled, TurnCompleted];
    let state1 = replay(&events);
    let state2 = replay(&events);
    assert_eq!(state1, state2);  // 验证一种情况
}

// Property-Based Testing：生成随机场景
proptest! {
    #[test]
    fn test_replay_deterministic(seed: u64) {
        // 用种子生成随机事件序列
        let events = generate_random_events(seed);
        
        // 核心不变量：重放两次必然相同
        let state1 = replay(&events);
        let state2 = replay(&events);
        assert_eq!(state1, state2);
        
        // 额外不变量：前缀重放也应该一致
        for i in 0..events.len() {
            let prefix = &events[..i];
            let s1 = replay(prefix);
            let s2 = replay(prefix);
            assert_eq!(s1, s2);
        }
    }
}
// 运行 100 次，每次不同的随机种子
```

**发现的真实 bug**:
- 某些事件顺序下 `last_seq` 不单调递增
- 空输入的 Turn 导致 Reducer panic
- 并发取消 + 审批的竞态条件

#### 跨进程集成测试

```rust
#[tokio::test]
async fn test_cross_process_cancel() {
    // 进程 A：启动一个长时间运行的 turn
    let mut session_a = spawn_session_a();
    let turn_id = session_a.start_turn("sleep 60").await?;
    
    // 进程 B：模拟另一个进程读取同一会话
    let mut session_b = Session::resume(same_session_id).await?;
    
    // 进程 B 发起取消
    session_b.cancel_turn(turn_id).await?;
    
    // 进程 A 尝试追加事件，应该发现日志已变
    let result = session_a.append_event(ToolCompleted { .. }).await;
    assert!(matches!(result, Err(SequenceMismatch)));
}
```

**验证的场景**:
- 进程 A append、进程 B cancel 的竞态
- 进程 B 崩溃留下半行 JSON，进程 A cache 失效
- 多个进程并发 append，序列号冲突检测

**我的项目验证**: 
- 65+ 单元测试覆盖各模块
- Property-Based Testing 验证核心不变量
- 跨进程集成测试覆盖并发场景
- 全部离线可运行（无网络依赖）

---

### 8. 架构自审与改进意识

**关键理念**: 没有完美的架构，只有适合当前阶段的权衡

#### 我识别的核心问题（12 个）

**P0 - 正确性风险**:
1. Approval 期间新输入导致 context 陈旧（可能执行过时的危险操作）
2. Context bytes 限制同时约束 history 和 request（合法对话被误拒）

**P1 - 架构债务**:
3. Agent Loop 职责过重（926 行单文件，难以扩展）
4. 工具串行执行（多个 read 无法并行，浪费 I/O 等待）
5. Provider Continuation 混入 Runtime 状态（泄漏实现细节）

**P2 - 功能缺失**:
6. Context 管理过于简单（不考虑内容重要性）
7. 工具输出截断过小（16KB 限制影响大文件处理）
8. Event Log 无压缩（长期运行会无限增长）
9. 缺少 Streaming 支持（用户体验差）

**P3 - 生产需求**:
10. 只有路径级边界检查（bash 仍可访问 /tmp、发起网络）
11. 不支持 Multi-Agent Workflow（无 Scheduler）
12. 不支持动态工具扩展（无 MCP 协议）

#### 改进优先级排序

| 优先级 | 问题 | 收益 | 成本 | 建议顺序 |
|--------|------|------|------|----------|
| P0 | Context 陈旧 | 防止危险操作 | M (2天) | 1 |
| P0 | Bytes 限制不准确 | 避免误拒 | S (1天) | 2 |
| P1 | 工具串行执行 | 3x 速度提升 | M (3天) | 3 |
| P2 | 输出截断小 | 改善体验 | S (0.5天) | 4 |
| P1 | Agent Loop 解耦 | 长期可维护性 | L (1周) | 5 |

#### 三阶段改进路线

**Phase 1 (1-2 周)**: 修复 P0，提升基础可用性  
**Phase 2 (1 个月)**: 架构重构（工具编排分离），支持并行执行  
**Phase 3 (2-3 个月)**: 添加生产特性（Context 管理、Streaming、沙箱）

**关键收获**: 
- 理解每个设计决策的权衡（正确性 vs 性能 vs 复杂度）
- 知道"简化版"与"生产级"的差距在哪
- 展示持续改进意识（不是"做完就算"）

---

## 面试核心话术（体现深度理解）

### 开场介绍（2 分钟）

"我从零实现了一个 Agent Harness 核心运行时，大约 8k 行 Rust 代码。这个项目的目标不是复制现有系统，而是**深入理解 Harness 的本质问题**。

Harness 是'模型循环之外的全部系统'，核心挑战有四个：
1. **状态如何在不可靠环境中保持一致**（我用 Event Sourcing 解决）
2. **工具如何安全执行且可控制**（进程组取消 + 三层输出限制）
3. **模型-工具循环如何推进**（Agent Loop + Context 管理）
4. **如何抽象不同 Provider**（统一接口 + 契约测试）

实现后我做了深度架构审查，识别了 12 个问题并给出优先级改进方案。这个过程让我理解了**为什么生产级 Harness 要这样设计**，而不只是'写了一个能跑的系统'。"

### 核心亮点话术

**当问到 Event Sourcing**:
"我选择 Event Sourcing 不是因为它流行，而是因为它精确匹配 Agent 系统的核心需求：**在不可靠环境中保证状态一致性**。传统的内存状态 + 定期 checkpoint 方案，进程 kill -9 后会丢失 checkpoint 间的状态。Event Sourcing 把每个状态变化都记录为事件，重放就能完整恢复。

我用 Property-Based Testing 验证了核心不变量：生成 24 种随机事件序列，每个都重放两次，状态必然相同。这不是单元测试能发现的——我发现了某些事件顺序下 `last_seq` 不单调递增的 bug，就是这个测试抓到的。"

**当问到进程管理**:
"进程管理的核心挑战是**子进程树清理**。传统的 `kill(pid)` 只杀父进程，bash 启动的子进程会泄漏。我用进程组级取消：Unix 下 `kill(-pgid, SIGKILL)` 一次性清理整个树。

另一个挑战是**输出防爆**。单一限制不够——一个工具耗尽配额，后续工具就没输出了。我实现了三层限制：per-tool、cumulative（跨工具共享）、UTF-8 安全截断。Cumulative limit 用 `AtomicUsize` + CAS 实现并发分配，避免了全局锁。"

**当问到取消机制**:
"中途取消的正确语义是**取消本身也是一个 durable 事件**。不能只设置内存 flag，进程崩溃后不知道取消是否生效。

我的实现是：进程 A 写入 `CancelRequested` 事件，进程 B 下次追加事件前检测到日志变化，读取发现取消，主动停止。这是 Fail-Stop 语义——发现冲突时拒绝追加，而非覆盖。

我写了跨进程集成测试验证这个场景：进程 A cancel、进程 B 正在运行，验证进程 B 正确检测到取消并停止。"

**当问到改进方向**:
"完成后我做了深度架构审查，识别了 12 个问题。最关键的是两个 P0 正确性问题：

1. **Approval 期间 Context 陈旧**：用户在审批期间提交了'取消'，但代码没检查 `pending_inputs`，审批通过后仍然用旧 context 执行工具。修复很简单——审批前检查队列是否为空，但这是很容易忽略的边界 case。

2. **工具串行执行**：当前循环是 `for call in calls { execute(call).await }`，多个 read 工具无法并行，浪费 I/O 等待。生产级需要依赖分析 + 拓扑排序并行调度。

我给出了三阶段改进路线，估算了每个任务的实施成本。这展示了我不只是'写完代码'，而是**持续思考如何改进**。"

---

## 为什么这个项目展示了对 Harness 的理解？

### 1. 抓住了本质问题
- ✅ 不是"调用 OpenAI API 写个聊天机器人"
- ✅ 而是"在不可靠环境中如何保证状态一致性、工具安全执行、循环正确推进"

### 2. 做出了正确的架构选择
- ✅ Event Sourcing（不是因为流行，而是因为匹配需求）
- ✅ 进程组取消（理解了为什么 `kill(pid)` 不够）
- ✅ Durable Cancel（理解了为什么取消也是事件）

### 3. 验证了核心不变量
- ✅ Property-Based Testing 验证重放确定性
- ✅ 跨进程集成测试验证并发一致性
- ✅ 契约测试验证 Provider 行为

### 4. 展示了改进意识
- ✅ 识别了 12 个问题并排序
- ✅ 给出了三阶段改进路线
- ✅ 知道"简化版"与"生产级"的差距

### 5. 可以深入讨论细节
- ✅ 为什么 Reducer 必须确定性？
- ✅ UTF-8 截断为什么不能从中间切？
- ✅ 序列号验证如何防止并发覆盖？
- ✅ Provider cursor 为什么能避免重复缓存？

---

## 最终建议

**简历中突出**:
1. "从零实现 Agent Harness 核心能力"（不是玩具）
2. "Event Sourcing + Property-Based Testing 验证"（架构 + 质量）
3. "进程组取消 + 三层输出限制"（工程细节）
4. "架构审查识别 12 个问题 + 改进方案"（反思能力）

**面试中强调**:
1. **理解本质问题**（不只是实现功能）
2. **架构选择有依据**（不是拍脑袋）
3. **测试策略深度**（Property-Based + 跨进程）
4. **持续改进意识**（不是做完就算）

**核心优势**:
- 对比"写了个 Agent 应用"的候选人 → 你理解**基础设施层**
- 对比"调研了很多论文"的候选人 → 你**从零实现**了核心能力
- 对比"没做过 Agent"的候选人 → 你有**完整的端到端经验**
