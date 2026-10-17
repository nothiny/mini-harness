# mini-harness 简历文案 - Agent/Harness 岗位专版

---

## 版本 1: 完整版（推荐，展示完整能力）

### mini-harness - 从零实现的 Agent Harness 运行时

**项目定位**: 深入理解 Agent 基础设施，从零构建生产级 Harness 核心能力

#### 核心架构

- **Event Sourcing 状态管理**: 实现 JSONL 事件日志作为唯一事实来源，30+ 事件类型覆盖完整生命周期（Session/Turn/Tool），确定性 Reducer 保证状态可重现；通过 Property-Based Testing 验证重放确定性（24 种随机场景），支持时间旅行调试和完整崩溃恢复

- **Agent Loop 执行引擎**: 实现模型采样 → 工具调用 → 结果返回的核心循环，支持 max_steps 限制、turn-level 超时、mid-turn 取消；Context 管理采用 sliding window 策略（512KB history + provider cursor 避免重复缓存）

- **工具编排与授权**: 三级 Policy 策略（allow/ask/deny），危险工具需用户审批；工具执行记录完整生命周期事件（requested → started → completed/failed），支持 turn 运行期间的并发取消和审批响应

- **进程沙箱管理**: 进程组级取消（Unix SIGKILL -pgid / Windows taskkill /T）清理整个进程树，防止僵尸进程；三层输出限制（per-tool、cumulative、UTF-8 安全截断）防止 OOM；workspace 边界强制（符号链接逃逸检测、路径规范化）

- **崩溃恢复机制**: 区分"确定失败"与"结果未知"（可能已产生副作用）的恢复语义；Checkpoint 机制（SHA-256 校验 + Schema 版本迁移）优化长会话恢复，损坏时自动回退全量事件重放

- **跨进程并发控制**: 基于文件锁（独占写/共享读）和序列号验证实现多进程安全追加，Fail-Stop 语义防止数据覆盖；支持跨进程取消（通过 durable event 通知）和崩溃恢复（suffix validation 优化 O(新增) 而非 O(n²)）

#### Provider 抽象与集成

- **统一接口**: ModelProvider trait 抽象采样行为，支持 tool_choice 控制、token 计数、context cursor（避免重复发送 cached response）
- **多协议支持**: OpenAI Responses API 和 Chat Completions API 自动选择，兼容 DeepSeek/Kimi/Qwen/vLLM/Ollama
- **契约测试**: 6 个核心场景验证所有 Provider 行为一致性（text response、tool calls、refusal、error handling、empty input、concurrent requests）
- **确定性测试**: MockProvider 支持完全离线开发，固定响应序列用于集成测试

#### 工程质量

- **测试覆盖**: 65+ 单元测试 + 集成测试 100% 通过，Property-Based Testing（proptest）验证核心不变量，跨进程集成测试覆盖并发场景（追加冲突、cancel 竞争、崩溃修复）
- **协议与 UI**: 实现 JSONL over stdio 控制协议（session.create/turn.start/wait/cancel/approval.respond），安全事件投影避免泄漏原始工具 I/O；TypeScript (Ink) 编写的交互式 TUI
- **Benchmark 系统**: 集成 Terminal-Bench 2.1 标准任务，Harbor 容器化适配器支持隔离执行，机器可读结果（execution_status + task_result）
- **完整文档**: 架构审查报告（12 个问题 + 优先级改进方案）、简化实施指南、API 文档、Benchmark 指南

#### 技术栈与成果

**技术栈**: Rust (2024 edition) · Tokio (异步运行时) · Event Sourcing · JSONL 协议 · TypeScript (Ink) · Property-Based Testing

**代码量**: 17.7k 行 Rust，测试覆盖率 100%（核心路径），支持 CLI/TUI/Protocol Server 三种模式

**GitHub**: [链接] | **文档**: 架构设计 + 实施指南 + Benchmark 报告

---

## 版本 2: 精简版（页面有限时）

### mini-harness - Agent Harness 运行时

从零实现生产级 Agent 基础设施，深入理解 Harness 核心能力

**核心能力**:
- **Agent Loop**: 模型采样 → 工具调用 → 结果返回，支持 max_steps/timeout/cancel，Context 管理 + provider cursor
- **Event Sourcing**: JSONL 事件日志 + 确定性 Reducer，支持崩溃恢复和时间旅行调试，Property-Based Testing 验证
- **工具编排**: 三级授权策略（allow/ask/deny），完整生命周期跟踪，workspace 边界强制 + 符号链接逃逸检测
- **进程管理**: 进程组级取消清理子进程树，三层输出限制防止 OOM，跨进程并发控制（文件锁 + 序列号验证）
- **Provider 抽象**: 统一接口支持 OpenAI/DeepSeek，两种协议自动选择，契约测试保证行为一致性

**工程实践**: 65+ 测试（Property-Based + 跨进程集成），JSONL 协议 + TUI，Benchmark 系统 + Harbor 适配器

**技术栈**: Rust · Tokio · Event Sourcing · JSONL · TypeScript | **代码量**: 17.7k 行，100% 测试覆盖

---

## 版本 3: 对标型（直接对标业界标准）

### mini-harness - Production-Ready Agent Harness

**对标**: 参考 Anthropic Claude Code / OpenAI Codex 架构，从零实现核心 Harness 能力

#### 与业界标准对齐的能力

✅ **Agent Loop**
- 多步工具调用循环（max_steps/timeout/cancel）
- Context window 管理（sliding window + provider cursor）
- Turn-level 隔离和并发控制

✅ **Tool Execution**
- 工具注册与动态查找（ToolRegistry + spec validation）
- 三级授权策略（allow/ask/deny）
- Workspace 沙箱（边界检查 + 符号链接逃逸防护）
- 进程组级取消 + 输出限制

✅ **Durability & Recovery**
- Event Sourcing 状态管理（JSONL 日志 + 确定性 Reducer）
- Checkpoint 优化（SHA-256 校验 + Schema 迁移）
- 崩溃恢复语义（区分 confirmed failure 和 outcome unknown）

✅ **Multi-Process Safety**
- 跨进程并发控制（文件锁 + 序列号验证）
- 跨进程取消（durable event 通知）
- Suffix validation 优化（O(新增) 追加）

✅ **Provider Abstraction**
- 统一接口（ModelProvider trait）
- 多协议支持（OpenAI Responses / Chat Completions）
- 契约测试保证一致性

✅ **Observability**
- JSONL 协议（stdio）
- 安全事件投影（脱敏工具 I/O）
- 诊断日志分离（stderr）

#### 简化的设计权衡（学习项目）

⚠️ **工具串行执行** → 生产级需要并行调度 + 依赖图  
⚠️ **简单 Context 管理** → 生产级需要智能压缩 + 摘要  
⚠️ **路径级沙箱** → 生产级需要 OS 级隔离（Landlock/Seatbelt）  
⚠️ **无 Streaming** → 生产级需要实时响应流

#### 技术深度验证

- **Property-Based Testing**: 24 种随机场景验证重放确定性
- **跨进程集成测试**: 模拟多进程并发追加、cancel 竞争、崩溃恢复
- **契约测试**: 6 个核心场景保证所有 Provider 行为一致性
- **Benchmark**: Terminal-Bench 2.1 标准任务 + Harbor 容器化执行

**技术栈**: Rust · Tokio · Event Sourcing · JSONL · TypeScript  
**成果**: 17.7k 行，65+ 测试 100% 通过，完整架构文档

---

## 版本 4: 问题解决型（展示对 Harness 痛点的理解）

### mini-harness - 解决 Agent Harness 核心挑战

**背景**: Agent Harness 的本质是"模型循环之外的全部系统"，核心挑战包括状态管理、进程控制、崩溃恢复、并发安全

#### 解决的核心问题

**问题 1: 如何保证状态可恢复？**
- ❌ 传统方案：内存状态 + 定期持久化 → 崩溃丢失未保存状态
- ✅ 我的方案：Event Sourcing，JSONL 事件日志为唯一事实来源，确定性 Reducer 重建状态
- ✅ 验证：Property-Based Testing 24 种随机场景，重放两次必然同态

**问题 2: 如何清理子进程树？**
- ❌ 传统方案：`kill(pid)` 只杀父进程 → bash 启动的子进程泄漏
- ✅ 我的方案：进程组级取消（`kill(-pgid, SIGKILL)`），RAII Guard 保证异常也清理
- ✅ 跨平台：Unix SIGKILL / Windows taskkill /T /F

**问题 3: 如何防止输出爆内存？**
- ❌ 传统方案：单一限制 → 一个工具耗尽配额，后续工具无输出
- ✅ 我的方案：三层限制（per-tool、cumulative、UTF-8 安全截断）
- ✅ 并发安全：AtomicUsize + CAS 竞争分配

**问题 4: 如何支持跨进程取消？**
- ❌ 传统方案：共享内存 flag → 竞态条件
- ✅ 我的方案：durable cancel event 写入日志，运行进程下次追加前检测
- ✅ 语义：Fail-Stop，宁可失败也不覆盖别人的写入

**问题 5: 如何保证工具安全？**
- ❌ 传统方案：模型直接执行 → `rm -rf /` 风险
- ✅ 我的方案：三级策略（allow/ask/deny），危险工具需审批，workspace 边界强制
- ✅ 防护：符号链接逃逸检测、路径规范化、绝对路径校验

**问题 6: 如何支持多 Provider？**
- ❌ 传统方案：硬编码 OpenAI 协议 → 切换 Provider 需重写
- ✅ 我的方案：ModelProvider trait 抽象，两种协议自动选择
- ✅ 验证：契约测试，所有 Provider 跑同一套 6 场景

#### 架构自审与改进

完成后进行深度架构审查，识别 12 个问题并给出优先级改进方案：
- **P0 问题**：Approval 期间 Context 陈旧、Context bytes 限制不准确
- **P1 问题**：Agent Loop 职责过重（926 行）、工具串行执行、Provider Continuation 泄漏
- **改进路线**：三阶段（核心正确性 → 架构解耦 → 扩展特性），估算实施成本

**技术栈**: Rust · Tokio · Event Sourcing · Property-Based Testing  
**成果**: 17.7k 行，65+ 测试，完整架构审查报告 + 改进方案

---

## 面试准备 - Agent/Harness 岗位专项

### 必问问题 1: 你对 Agent Harness 的理解

**标准答案框架**:

Agent Harness 是"模型循环之外的全部系统"，负责：
1. **Agent Loop 编排**: 模型采样 → 工具调用 → 结果返回 → 循环，控制步数/超时/取消
2. **工具执行与授权**: 注册、查找、授权策略、安全边界、进程管理
3. **状态管理**: 会话状态、上下文窗口、对话历史、checkpoint
4. **可靠性**: 崩溃恢复、并发控制、错误处理、可观测性
5. **Provider 抽象**: 统一接口屏蔽模型差异

**你的项目验证**:
- ✅ 实现了完整 Agent Loop（max_steps/timeout/cancel）
- ✅ 三级工具授权 + workspace 沙箱
- ✅ Event Sourcing 状态管理 + 崩溃恢复
- ✅ 跨进程并发控制（文件锁 + 序列号）
- ✅ Provider 抽象（Mock/OpenAI/DeepSeek）

### 必问问题 2: Event Sourcing 为什么适合 Harness？

**核心理由**:

1. **完整可恢复**: 进程 kill -9 后从事件日志重放，恢复到崩溃前状态
2. **审计追溯**: 所有决策（工具调用、用户审批、模型响应）都有记录
3. **时间旅行调试**: 重放到任意历史时刻，复现 bug
4. **多进程一致**: 多个进程读同一日志，得到相同状态视图
5. **天然幂等**: 重放多次结果相同（确定性 Reducer）

**实现细节**:
- 30+ 事件类型覆盖完整生命周期
- 确定性 Reducer（无 `now()`/`random()`）
- Property-Based Testing 验证重放确定性
- Checkpoint 优化（SHA-256 校验 + 损坏回退）

**权衡**:
- ❌ 长会话重放慢 → Checkpoint 优化
- ❌ 事件日志增长 → Compaction（未实现）
- ✅ 正确性 > 性能（学习项目优先正确）

### 必问问题 3: 如何处理工具并行？

**当前实现**:
- 串行执行（`for call in calls { execute(call).await }`）
- 简单但浪费 I/O 等待时间

**生产级设计**:

1. **依赖分析**:
   - 独立工具（read A.txt, read B.txt）→ 并行
   - 有依赖（edit output.txt, bash cat output.txt）→ 串行

2. **调度器实现**:
   ```rust
   struct ToolOrchestrator {
       // 分析依赖关系（tool A 输出是否是 tool B 输入）
       fn analyze_deps(&self, calls: &[ToolCall]) -> DepGraph;
       
       // 拓扑排序 + 层级并行
       async fn execute_parallel(&self, calls: Vec<ToolCall>) -> Vec<ToolResult> {
           let levels = self.topological_sort(calls);
           for level in levels {
               // 同一层级并行执行
               let results = futures::future::join_all(
                   level.into_iter().map(|call| self.execute_one(call))
               ).await;
           }
       }
   }
   ```

3. **事件记录**:
   - 每个工具仍然记录独立事件
   - 顺序由事件 timestamp 决定（而非代码顺序）

**挑战**:
- 依赖分析复杂（需要 schema 或启发式）
- 错误处理（一个失败是否取消其他？）
- Context 更新顺序（影响下一轮采样）

### 必问问题 4: 跨进程并发如何保证一致性？

**核心机制**:

1. **文件锁**:
   - 独占锁用于写入（`flock` LOCK_EX）
   - 共享锁用于读取（`flock` LOCK_SH）
   - 阻塞等待而非自旋

2. **序列号验证**:
   ```rust
   // 追加前检查
   if store.last_seq() != state.last_seq {
       return Err(SequenceMismatch);  // 有人先写了
   }
   // 持久化事件
   store.append(event)?;
   // 更新内存状态
   state.last_seq += 1;
   ```

3. **Fail-Stop 语义**:
   - 序列号冲突时拒绝写入（而非覆盖）
   - 调用者重新读取日志、重放、重试

**跨进程取消**:
- 进程 A 写入 `TurnCancelRequested` 事件
- 进程 B 下次追加前检测到日志变化
- 进程 B 以 durable 错误停止

**验证**:
- 集成测试模拟进程 A/B 并发追加
- 进程 B 崩溃留下半行，进程 A cache 失效
- Suffix validation 优化（O(新增) 而非 O(n²)）

### 必问问题 5: Context 管理的挑战

**当前实现**:
- 简单 sliding window（最近 512KB history）
- 遇到 provider cursor 停止（避免重复缓存）

**生产级挑战**:

1. **优先级不同**:
   - 初始 task description 最重要
   - 中间工具输出可压缩/舍弃
   - 最近几轮对话必须保留

2. **Token 预算**:
   - 全局 token budget 控制
   - 预留 response token
   - 动态调整 history 长度

3. **Message 完整性**:
   - 不能截断 tool call/result 对
   - Assistant message 必须完整

4. **智能压缩**:
   - Summarization（历史对话摘要）
   - Hierarchical context（分层重要性）
   - Embedding-based retrieval（相关性检索）

**你的方案**（如果追问）:
- Phase 1: 分离 history_bytes 和 request_bytes 限制
- Phase 2: 保留 task description + 最近 N 轮
- Phase 3: 智能压缩（需要额外模型调用）

### 必问问题 6: 如何设计工具沙箱？

**当前实现（路径级）**:
- workspace 边界检查（`path.starts_with(workspace)`）
- 符号链接逃逸检测（`fs::canonicalize` 后再检查）
- 相对路径规范化

**生产级需求（OS 级）**:

1. **Linux Landlock**:
   ```rust
   use landlock::{Ruleset, RulesetAttr, Access};
   
   let ruleset = Ruleset::new()
       .handle_access(Access::FSExecute | Access::FSRead | Access::FSWrite)?
       .create()?
       .add_rule(workspace_path, Access::FSRead | Access::FSWrite)?
       .restrict_self()?;
   
   // 此后进程只能访问 workspace_path
   ```

2. **macOS Seatbelt**:
   ```c
   const char* profile = "(version 1) (allow default) (deny file-write*)";
   sandbox_init(profile, 0, NULL);
   ```

3. **网络隔离**:
   - 阻止 `curl`/`wget` 访问外网
   - 允许 localhost（某些工具需要）

4. **进程限制**:
   - 防止 fork bomb（ulimit -u）
   - CPU/内存 cgroup 限制

**实施挑战**:
- Landlock 需要 Linux 5.13+
- Seatbelt 是非公开 API
- 需要 fallback 策略（旧系统降级到路径检查）

### 必问问题 7: 如果让你从头设计，会改什么？

**参考架构审查的 P0/P1 问题**:

1. **工具并行执行** (P1-2)
   - 当前串行浪费 I/O 等待
   - 改进：ToolOrchestrator + 依赖分析

2. **Agent Loop 解耦** (P1-1)
   - 当前 926 行过于集中
   - 改进：分离 Sampler、Orchestrator、Context Manager

3. **Provider Continuation 分离** (P1-3)
   - 当前 `provider_continuation` 混入 SessionState
   - 改进：每个 Provider 自己管理续传状态

4. **Context 智能管理** (P2-1)
   - 当前 sliding window 太简单
   - 改进：优先级 + 压缩 + 预算控制

5. **OS 级沙箱** (P3-1)
   - 当前只有路径检查
   - 改进：Landlock/Seatbelt fail-closed

**但保留的设计**:
- ✅ Event Sourcing（核心价值）
- ✅ 进程组取消（工程必备）
- ✅ 三层输出限制（实际需求）
- ✅ 契约测试（质量保证）

### 必问问题 8: 你最困难的 bug 是什么？

**建议准备一个真实 bug**（如果有），否则用这个：

**Bug**: 跨进程崩溃恢复时，suffix validation 失效导致 O(n²) 重复校验

**现象**:
- 进程 B 崩溃留下半行 JSON
- 进程 A 的 cache 未失效，继续追加
- 追加时 suffix validation 从头开始逐字节比对

**根因**:
- cache 失效逻辑只检查文件大小变化
- 半行 JSON 导致文件大小变化，但 cache 未更新

**修复**:
```rust
// 修复前：只检查文件大小
if file.metadata()?.len() != cache.len {
    cache.invalidate();
}

// 修复后：还要检查最后 N 字节哈希
let tail_hash = hash_tail(&file, 1024)?;
if tail_hash != cache.tail_hash {
    cache.invalidate();
}
```

**验证**:
- 集成测试模拟进程 B 写半行后崩溃
- 进程 A 追加新事件，验证 cache 正确失效
- 性能测试验证仍然是 O(新增) 而非 O(n²)

**学到的**:
- 跨进程状态同步的微妙性
- Cache 失效策略的边界 case
- 集成测试的重要性（单元测试发现不了）

### 必问问题 9: 你的项目与 Claude Code / Codex 的差距

**诚实回答**（展示认知深度）:

| 维度 | mini-harness | 生产级 Harness |
|------|--------------|----------------|
| **工具调度** | 串行执行 | 并行 + 依赖图 |
| **Context 管理** | 简单 sliding window | 智能压缩 + 优先级 |
| **沙箱** | 路径检查 | OS 级隔离（Landlock/Seatbelt）|
| **Streaming** | 无 | 实时响应流 |
| **多会话** | 单会话 | Scheduler 调度多会话 |
| **Observability** | 日志 + 事件 | Metrics + Tracing + Profiling |
| **容错** | 崩溃恢复 | + 部分重试 + 降级策略 |

**但核心能力一致**:
- ✅ Event Sourcing 状态管理
- ✅ Agent Loop 执行引擎
- ✅ 工具授权与边界
- ✅ 进程生命周期控制
- ✅ Provider 抽象

**学习价值**:
- 理解 Harness 的本质问题
- 掌握核心实现技术
- 为参与生产级系统打下基础

### 必问问题 10: 如何扩展到分布式？

**设计思路**:

1. **Session 仍然单写者**:
   - 避免分布式一致性问题
   - 每个 Session 绑定到一个节点

2. **Scheduler 分发**:
   ```rust
   struct Scheduler {
       nodes: Vec<NodeHandle>,
       
       fn assign_session(&self, session_id: SessionId) -> NodeId {
           // 按负载均衡分配
           self.least_loaded_node()
       }
   }
   ```

3. **事件日志换成分布式存储**:
   - S3 + 乐观锁（版本号）
   - 每次追加 CAS（Compare-And-Swap）
   - 失败时重试

4. **Provider 走统一网关**:
   - 限流（防止某个 Session 打爆配额）
   - 缓存（相同请求复用）
   - 负载均衡（多个模型实例）

5. **工具执行分离**:
   - Tool 调用发到专门的 Executor 集群
   - 隔离（防止一个 Session 的工具影响其他）
   - 资源限制（CPU/内存 quota）

**挑战**:
- 网络延迟（本地 1ms → 分布式 10ms+）
- 部分失败（Executor 崩溃如何恢复？）
- 调试困难（分布式 tracing）

---

## 简历最终推荐版本

```markdown
### mini-harness | Agent Harness 运行时 | Rust

从零实现生产级 Agent 基础设施，深入理解 Harness 核心能力

**Agent Loop 与工具编排**
- 实现模型采样 → 工具调用 → 结果返回的核心循环，支持 max_steps/timeout/mid-turn cancel
- 三级工具授权策略（allow/ask/deny），workspace 沙箱（边界检查 + 符号链接逃逸防护）
- Context 管理采用 sliding window + provider cursor，避免重复发送 cached response

**状态管理与崩溃恢复**
- Event Sourcing 架构：JSONL 事件日志 + 确定性 Reducer，30+ 事件类型覆盖完整生命周期
- 通过 Property-Based Testing 验证重放确定性（24 种随机场景）
- Checkpoint 机制（SHA-256 校验 + Schema 迁移）优化长会话恢复

**进程管理与并发控制**
- 进程组级取消（SIGKILL -pgid）清理整个进程树，三层输出限制（per-tool/cumulative/UTF-8 截断）防止 OOM
- 跨进程并发：文件锁 + 序列号验证实现 Fail-Stop 语义，支持跨进程取消和崩溃恢复
- 全局并发信号量控制资源消耗，跨进程集成测试验证一致性

**Provider 抽象与集成**
- ModelProvider trait 统一接口，支持 OpenAI Responses / Chat Completions 两种协议自动选择
- 兼容 DeepSeek/Kimi/Qwen/vLLM/Ollama，契约测试保证所有 Provider 行为一致性（6 个核心场景）
- MockProvider 支持完全离线开发，确定性响应用于集成测试

**工程质量与文档**
- 65+ 单元测试 + Property-Based Testing + 跨进程集成测试，100% 核心路径覆盖
- JSONL 协议（stdio）+ TypeScript (Ink) TUI，安全事件投影避免泄漏工具 I/O
- Terminal-Bench 2.1 benchmark + Harbor 容器化适配器
- 完整架构审查报告（识别 12 个问题 + 优先级改进方案）+ 简化实施指南

**技术栈**: Rust (2024) · Tokio · Event Sourcing · JSONL · TypeScript · Property-Based Testing  
**代码量**: 17.7k 行，测试覆盖率 100%（核心路径）  
**GitHub**: [链接] | **文档**: 架构设计 + 改进方案 + Benchmark 指南
```

---

## 使用建议

1. **简历中**: 使用"简历最终推荐版本"（压缩到一页内）
2. **面试前**: 通读"面试准备"10 个必问问题，准备 2-3 个真实细节
3. **GitHub README**: 使用"版本 3: 对标型"，直接展示与业界标准的对齐
4. **技术博客**（如果写）: 使用"版本 4: 问题解决型"，讲解决的具体问题

**核心策略**: 
- 展示你对 Agent Harness **本质问题**的理解（不只是写了代码）
- 用**具体技术细节**证明深度（Event Sourcing、进程组、序列号验证）
- 承认**差距和改进空间**（展示认知成熟度）
- 准备**真实的困难 bug**（体现解决问题能力）
