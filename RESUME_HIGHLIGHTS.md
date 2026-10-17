# mini-harness 简历项目亮点

---

## 版本 1: 技术深度型（适合基础设施/系统工程岗位）

### 项目名称
**mini-harness** - 从零实现的 Agent Harness 运行时

### 一句话描述
基于 Event Sourcing 和 Actor Model 的可恢复 Agent 执行引擎，支持工具调用、进程管理、崩溃恢复和跨进程并发控制

### 核心亮点

- **Event Sourcing 架构**: 实现 JSONL 事件日志作为唯一事实来源，通过确定性 Reducer 重建状态，支持时间旅行调试和完整崩溃恢复；通过 Property-Based Testing 验证重放确定性（24 种随机场景）

- **进程生命周期管理**: 实现进程组级取消（Unix SIGKILL -pgid / Windows taskkill /T），三层输出限制防止 OOM（per-tool、cumulative、UTF-8 安全截断），全局并发信号量控制资源消耗

- **跨进程并发控制**: 基于文件锁和序列号验证实现多进程安全写入，Fail-Stop 语义防止数据覆盖；支持跨进程取消和崩溃恢复，通过集成测试验证一致性

- **Actor 并发模型**: Single-Writer Session Actor 通过 tokio::select! 处理并发命令（StartTurn/Cancel/Approval），Durable Queue 确保运行期间提交的输入按 FIFO 自动执行且不丢失

- **可插拔 Provider 抽象**: 统一接口支持 MockProvider（确定性测试）和 OpenAI/DeepSeek（两种 wire 协议自动选择），通过契约测试保证行为一致性（6 个核心场景）

### 技术栈
Rust (2024 edition) · Tokio (异步运行时) · Property-Based Testing · JSONL 协议 · Event Sourcing

### 技术成果
- 17.7k 行 Rust 代码，65+ 单元测试 + 多个集成测试 100% 通过
- 完整离线测试（无网络依赖），Property-Based Testing 覆盖核心不变量
- 支持 DeepSeek/OpenAI 等真实 LLM，提供 Ink (React for CLI) 交互式 TUI
- 集成 Terminal-Bench 2.1 benchmark 系统和 Harbor 容器化适配器

---

## 版本 2: 工程实践型（适合后端/全栈工程岗位）

### 项目名称
**mini-harness** - 生产级 AI Agent 执行引擎

### 一句话描述
支持工具调用、审批流程、崩溃恢复的 Agent Harness，可接入 DeepSeek/OpenAI 等真实 LLM

### 核心亮点

- **可靠性设计**: 实现基于事件溯源的崩溃恢复机制，进程 kill -9 后可从 JSONL 日志完整恢复会话状态；区分"确定失败"与"结果未知"（可能已产生副作用）的恢复语义

- **安全执行**: 工具调用支持三级策略（allow/ask/deny），危险操作需用户审批；workspace 边界强制（符号链接逃逸检测）、进程组级取消防止子进程泄漏、输出限制防止内存溢出

- **真实 LLM 集成**: 支持 OpenAI Responses 和 Chat Completions 两种协议，自动适配 DeepSeek/Kimi/Qwen 等兼容服务；MockProvider 支持完全离线开发和测试

- **完整测试覆盖**: 65+ 单元测试 + 集成测试，覆盖进程管理、事件重放、跨进程并发、工具边界检查、Provider 行为契约；支持 benchmark 评测标准任务性能

- **协议与 UI**: 实现 JSONL over stdio 控制协议（支持 session.create/turn.start/cancel/approval 等命令），提供 TypeScript (Ink) 编写的交互式 TUI，安全事件投影避免泄漏原始工具 I/O

### 技术栈
Rust · Tokio · Event Sourcing · JSONL 协议 · TypeScript (Ink) · Docker (Harbor 适配器)

### 项目成果
- 从零实现完整 Agent Harness（~18k 行代码），支持生产级特性（崩溃恢复、工具审批、跨进程并发）
- 完整文档（架构设计、API 文档、Benchmark 指南），全部测试离线可运行
- 支持 CLI、TUI、Protocol Server 三种使用模式，可集成到其他系统

---

## 版本 3: 架构设计型（适合架构师/Tech Lead 岗位）

### 项目名称
**mini-harness** - 可扩展的 Agent 基础设施

### 一句话描述
从零设计并实现的 Agent Harness，采用 Event Sourcing + Actor Model 架构，支持工具执行、崩溃恢复、多进程并发

### 架构亮点

- **Event Sourcing 为核心**: 设计 30+ 事件类型覆盖完整生命周期（Session/Turn/Tool），确定性 Reducer 保证状态可重现；Checkpoint 机制（SHA-256 校验 + Schema 版本迁移）优化重放性能，损坏时自动回退全量重放

- **分层架构**: 严格分离关注点（Durable 层负责持久化、Runtime 层负责循环逻辑、Executor 层负责进程管理、Policy 层负责授权），每层独立测试；Provider/Tool 通过 trait 抽象，易于扩展

- **并发一致性**: Single-Writer Actor 避免竞态条件，跨进程通过文件锁 + 序列号验证保证顺序一致性；Durable Queue 处理运行期间提交的输入，序列化复杂并发场景

- **可观测性**: 诊断日志（stderr）与协议（stdout）严格分离，事件日志天然支持审计和回放调试；JSONL 协议支持安全事件投影（脱敏工具原始 I/O）

- **测试驱动**: Property-Based Testing 验证核心不变量（重放确定性、事件完整性），契约测试保证 Provider 行为一致性，跨进程集成测试覆盖并发场景；全部测试离线可运行，无外部依赖

### 技术决策

- **选择 Rust**: 类型安全防止状态损坏，所有权系统保证并发安全，零成本抽象支持高性能
- **选择 Event Sourcing**: 完整历史追溯、时间旅行调试、天然支持崩溃恢复
- **选择 Actor Model**: 消除共享状态竞争，通过消息传递简化并发逻辑
- **选择 JSONL 格式**: 人类可读、逐行追加、易于流式处理和增量解析

### 项目影响
- 作为学习项目完整实现 Agent Harness 核心概念，代码和文档可供团队学习参考
- 架构设计文档（ARCHITECTURE_REVIEW.md）深度分析 12 个问题并给出优先级改进方案
- 可作为生产系统原型，快速验证 Agent 工作流设计

---

## 版本 4: 简洁精炼型（适合一般工程岗位，篇幅受限）

### mini-harness - AI Agent 执行引擎

从零实现的 Agent Harness，支持工具调用、崩溃恢复、真实 LLM 接入

**核心技术**:
- 基于 Event Sourcing 的崩溃恢复（JSONL 日志 + 确定性重放）
- 进程组级取消、输出限制、workspace 边界检查
- 跨进程并发控制（文件锁 + 序列号验证）
- 支持 DeepSeek/OpenAI，提供 CLI/TUI/Protocol Server 三种模式

**技术栈**: Rust · Tokio · Event Sourcing · JSONL · TypeScript (Ink)

**成果**: 17.7k 行代码，65+ 测试 100% 通过，完整文档和 Benchmark 系统

---

## 版本 5: 成果导向型（适合强调产出的岗位）

### mini-harness - 生产级 AI Agent 基础设施

**项目背景**: 为理解 AI Agent 底层机制，从零实现完整 Agent Harness 运行时

**核心成果**:
- ✅ **完整实现**: 17.7k 行 Rust 代码，覆盖事件溯源、进程管理、崩溃恢复、工具授权全流程
- ✅ **高可靠性**: 支持 kill -9 崩溃恢复、跨进程并发安全、进程组级取消、三层输出限制
- ✅ **真实可用**: 接入 DeepSeek/OpenAI 真实 LLM，提供 CLI/TUI/Protocol Server 三种使用方式
- ✅ **高质量**: 65+ 测试 100% 通过（含 Property-Based Testing 和跨进程集成测试）
- ✅ **完整文档**: 架构设计文档、API 文档、Benchmark 指南、简化实施指南

**技术难点**:
- 跨进程并发一致性：通过文件锁 + 序列号验证实现 Fail-Stop 语义
- 进程组管理：跨平台实现（Unix SIGKILL / Windows taskkill）清理整个进程树
- 确定性重放：通过 Property-Based Testing 验证 24 种随机场景下的重放一致性

**技术栈**: Rust (2024 edition) · Tokio · Event Sourcing · JSONL 协议 · TypeScript

---

## 使用建议

### 选择哪个版本？

- **系统工程/基础设施岗位** → 版本 1（强调技术深度）
- **后端/全栈工程岗位** → 版本 2（强调工程实践）
- **架构师/Tech Lead** → 版本 3（强调架构设计）
- **简历篇幅有限** → 版本 4（简洁精炼）
- **强调产出和成果** → 版本 5（成果导向）

### 关键亮点选择

根据岗位 JD 调整重点：

**强调**可靠性 → Event Sourcing、崩溃恢复、跨进程并发  
**强调**性能 → 进程管理、输出限制、并发控制  
**强调**架构 → 分层设计、trait 抽象、测试覆盖  
**强调**工程化 → 完整文档、测试覆盖、Protocol 设计  
**强调**AI/LLM → Provider 抽象、工具调用、真实 LLM 集成

### 面试准备 - 可能被问到的问题

#### 技术细节类

Q: **为什么选择 Event Sourcing？**  
A: 三个核心原因：
1. 崩溃恢复 - 进程 kill -9 后从事件日志完整重放
2. 可调试性 - 完整历史追溯，时间旅行调试
3. 审计需求 - 所有决策都有记录（工具调用、用户审批）

Q: **如何保证跨进程并发安全？**  
A: 两层机制：
1. 文件锁（独占写、共享读）
2. 序列号验证（每次追加前检查 `store.last_seq == state.last_seq`）
3. Fail-Stop 语义（冲突时拒绝而非覆盖）

Q: **如何处理进程组清理？**  
A: 创建新进程组（Unix `setpgid(0,0)` / Windows `CREATE_NEW_PROCESS_GROUP`），取消时 `kill(-pgid)` 清理整个树；RAII Guard 确保异常情况也清理。

Q: **Event Sourcing 的性能瓶颈是什么？如何优化？**  
A: 
- 瓶颈：长会话需要重放大量事件
- 优化：Checkpoint 机制（快照 + SHA-256 校验），损坏时回退全量重放
- 权衡：Checkpoint 频率 vs 恢复速度（当前 50 事件一次）

#### 架构设计类

Q: **为什么使用 Actor Model？**  
A: 
- Single-Writer 避免共享状态竞争
- 通过 mpsc channel 序列化所有状态变更
- 简化并发逻辑（CancelTurn、ApproveTool 可在运行期间调用）

Q: **如果要支持工具并行执行，如何设计？**  
A: 
1. 提取 ToolOrchestrator 负责调度
2. 分析依赖关系（tool A 输出是否是 tool B 输入）
3. 独立工具用 `tokio::spawn` 并行，有依赖的串行
4. 统一等待所有工具完成后再进入下一轮

Q: **如何扩展到分布式场景？**  
A: 
- Session 仍然是单写者（避免分布式一致性问题）
- 多 Session 通过 Scheduler 调度到不同节点
- 事件日志可以换成分布式存储（S3 + 乐观锁）
- Provider 调用可以走统一网关（限流、缓存）

#### 工程实践类

Q: **测试策略是什么？**  
A: 三层：
1. 单元测试 - 每个模块独立测试（Reducer、Process、Tools）
2. Property-Based Testing - 验证核心不变量（重放确定性）
3. 集成测试 - 跨进程并发、完整 Agent Loop、崩溃恢复

Q: **如何调试复杂的并发问题？**  
A: 
1. 事件日志天然记录完整时序
2. 重放到问题点前一步，单步执行
3. 诊断日志分离到 stderr（`RUST_LOG=debug`）
4. 集成测试固定随机种子，可重现

Q: **代码质量如何保证？**  
A: 
- Clippy 强制检查（`-D warnings`）
- 格式化强制（`cargo fmt --check`）
- 测试覆盖率（65+ 测试，核心路径 100% 覆盖）
- 文档完整（API 文档 + 架构文档 + 实施指南）

#### 项目反思类

Q: **项目最大的技术挑战是什么？**  
A: 跨进程崩溃恢复的正确性验证 - 需要写集成测试模拟进程 B 崩溃留下半行，进程 A 的 cache 正确失效，修复后的连续追加走 suffix validation（O(新增) 而非 O(n²)）。

Q: **如果重新设计，会改什么？**  
A: （参考 ARCHITECTURE_REVIEW.md 的建议）
1. 工具执行改为并行（当前串行浪费 I/O 等待）
2. Agent Loop 职责分离（当前 926 行过于集中）
3. Provider Continuation 不应混入 Runtime 状态
4. 添加 OS 级沙箱（当前只有路径检查）

Q: **学到了什么？**  
A: 
1. Event Sourcing 在复杂状态管理中的威力
2. Rust 类型系统如何帮助保证并发安全
3. Property-Based Testing 如何发现边界 case
4. 架构设计的权衡（正确性 vs 性能 vs 复杂度）

---

## Markdown 格式（可直接复制到简历）

### 示例 1: 简洁格式

```markdown
### mini-harness - AI Agent 执行引擎

从零实现的 Agent Harness，基于 Event Sourcing 架构，支持工具调用、崩溃恢复、真实 LLM 接入

- **Event Sourcing 架构**: JSONL 事件日志 + 确定性 Reducer，支持 kill -9 崩溃恢复和时间旅行调试
- **进程生命周期管理**: 进程组级取消、三层输出限制、全局并发控制，防止子进程泄漏和 OOM
- **跨进程并发控制**: 文件锁 + 序列号验证实现多进程安全写入，Fail-Stop 语义防止数据覆盖
- **真实 LLM 集成**: 支持 DeepSeek/OpenAI，提供 CLI/TUI/Protocol Server 三种使用模式
- **完整测试**: 65+ 测试（Property-Based Testing + 跨进程集成测试），完整架构文档和实施指南

**技术栈**: Rust · Tokio · Event Sourcing · JSONL · TypeScript (Ink)  
**成果**: 17.7k 行代码，100% 测试通过，支持生产级特性（崩溃恢复、工具审批、并发控制）
```

### 示例 2: 分点格式

```markdown
### mini-harness | AI Agent 执行引擎 | Rust

从零实现的 Agent Harness 运行时，采用 Event Sourcing + Actor Model 架构

**技术亮点**:
- 实现基于 JSONL 的 Event Sourcing，确定性 Reducer 支持崩溃恢复和时间旅行调试
- 跨进程并发控制：文件锁 + 序列号验证，Fail-Stop 语义保证一致性
- 进程组级取消（SIGKILL -pgid）清理整个进程树，三层输出限制防止 OOM
- Actor 并发模型：Single-Writer + tokio::select! 处理并发命令，Durable Queue 保证消息不丢失
- 可插拔 Provider 抽象，统一接口支持 Mock/OpenAI/DeepSeek，通过契约测试保证一致性

**工程实践**:
- 65+ 单元测试 + Property-Based Testing 验证核心不变量，跨进程集成测试覆盖并发场景
- 完整文档（架构设计、实施指南、Benchmark 指南），代码质量通过 Clippy 强制检查
- 支持 CLI/TUI/Protocol Server 三种模式，JSONL 协议支持安全事件投影

**技术栈**: Rust (2024) · Tokio · Event Sourcing · JSONL · TypeScript  
**代码量**: 17.7k 行，测试覆盖率 100%（核心路径）
```

---

## GitHub README Badge 建议

在项目 README 顶部添加：

```markdown
![Rust](https://img.shields.io/badge/Rust-1.85+-orange?logo=rust)
![Tests](https://img.shields.io/badge/tests-65%20passing-brightgreen)
![Architecture](https://img.shields.io/badge/architecture-Event%20Sourcing-blue)
![License](https://img.shields.io/badge/license-MIT-green)
```

效果：增加专业感，快速传达项目质量
