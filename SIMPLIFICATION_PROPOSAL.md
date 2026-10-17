# mini-harness 简化方案

**当前状态**: ~17.7k 行 Rust 代码，对初学者门槛较高  
**目标**: 保留核心教学价值，降低理解成本

---

## 一、当前架构复杂度分析

### 1.1 必要复杂度（保留）

这些是 Agent Harness 的本质问题，**不能简化**：

✅ **Event Sourcing 核心** (~2500 行)
- `event.rs`: 事件定义（30+ 种事件类型）
- `reducer.rs`: 确定性状态重建
- `event_log.rs`: JSONL 追加日志

**教学价值**: Event Sourcing 是理解"如何从无到有构建可恢复系统"的关键

✅ **Agent Loop** (~1000 行核心逻辑)
- 模型采样 → 工具调用 → 结果返回 → 下一轮
- 取消、超时、步数限制

**教学价值**: 这是 Agent Harness 的心脏

✅ **进程执行** (~800 行)
- 进程组取消、输出限制、UTF-8 截断
- 跨平台差异（Unix SIGKILL / Windows taskkill）

**教学价值**: 实际生产环境必然遇到的问题

---

### 1.2 可选复杂度（可简化）

这些功能增加了代码量，但对**初学者理解核心概念**帮助有限：

#### 🔴 高复杂度，可大幅简化

**1. Checkpoint 系统** (~1200 行)
- `checkpoint.rs`: SHA-256 校验、Schema 版本迁移、压缩
- `recovery.rs`: 三种故障分类、未完成工作检测

**问题**:
- 初学者只需要理解"事件重放"，不需要理解"快照优化"
- Checkpoint 是性能优化，不是正确性保证

**简化方案**: 
- 删除 `checkpoint.rs` 和 `recovery.rs`（-1200 行）
- 只保留"从第一个事件开始重放"的朴素实现
- 在 README 注释"生产环境需要 checkpoint，但学习时不必要"

---

**2. Session Actor 并发模型** (~1300 行)
- `session_actor.rs`: 复杂的 tokio::select! 分支
- `session.rs`: SessionHandle + SessionCommand 抽象

**问题**:
- Actor 模式增加了理解成本
- 初学者难以理解"为什么需要 mpsc channel + command pattern"

**简化方案**:
- 保留 `Session` 直接调用接口（已有）
- 删除 `SessionActor` 和 `SessionCommand`（-800 行）
- README 说明："单线程版本足够理解核心概念，生产环境需要 Actor"

---

**3. Durable Queue** (~300 行逻辑)
- 运行期间提交的输入持久化后排队

**问题**:
- 边缘场景，初学者不会遇到
- "一次只运行一个 turn" 对学习来说足够

**简化方案**:
- Turn 运行期间直接拒绝新输入（返回 "busy" 错误）
- 删除 `pending_inputs` 和队列逻辑（-300 行）

---

**4. 复杂的 Tool Policy 系统** (~200 行)
- `allow` / `ask` / `deny` 三种策略
- Policy 拒绝与用户拒绝的区分

**问题**:
- 初学者只需要理解"用户可以拒绝工具调用"
- Policy 是企业级需求，不是教学核心

**简化方案**:
- 只保留 `allow` 和 `ask` 两种模式
- 所有拒绝统一为一种事件类型（-150 行）

---

#### 🟡 中等复杂度，可适度简化

**5. 跨进程并发控制** (~500 行)
- 文件锁 + 序列号验证
- 跨进程崩溃恢复测试

**问题**:
- 多进程场景对初学者来说过于高级
- 单进程版本足够理解核心

**简化方案**:
- 保留代码，但在文档中标记为"高级特性"
- 初学者可以先跳过这部分

---

**6. 多 Provider 支持** (~800 行)
- OpenAI Responses / Chat Completions 两种协议
- Mock / OpenAI / DeepSeek 三种实现

**问题**:
- 初学者只需要理解"如何调用一个模型"
- 协议细节是工程问题，不是架构问题

**简化方案**:
- 只保留 **MockProvider** 和 **一种真实 Provider**
- 删除协议选择逻辑（-300 行）

---

**7. Benchmark 系统** (~600 行)
- 任务清单、artifact 保存、机器可读结果
- Harbor 适配器

**问题**:
- 这是评测需求，不是 Harness 核心
- 初学者不需要 benchmark

**简化方案**:
- 移到独立 crate `mini-harness-benchmark`（不删除，但不放在核心代码）
- 主项目只保留 `run` 命令

---

**8. JSONL 协议服务器** (~1000 行)
- `protocol/jsonl.rs`: 完整的 stdio 协议实现
- `event_projection.rs`: 安全事件投影

**问题**:
- 初学者不需要理解"客户端-服务器架构"
- TUI 是锦上添花，不是核心

**简化方案**:
- 移到 `mini-harness-server` 独立 crate
- 主项目只保留 `Session::start_turn()` 同步接口

---

#### 🟢 低复杂度，保留

**9. Context 管理** (~200 行)
- 简单的 sliding window 实现

**建议**: 保留，这是 Agent 必然遇到的问题

**10. 工具注册** (~300 行)
- ToolRegistry + 三个核心工具

**建议**: 保留，工具是 Agent 的核心抽象

---

## 二、简化后的架构

### 2.1 核心保留（~8000 行）

```text
src/
├── main.rs               # 只保留 run/resume/demo 命令
├── lib.rs
├── durable/              # 删除 checkpoint.rs, recovery.rs
│   ├── event.rs          # ✅ 保留
│   ├── event_log.rs      # ✅ 保留（删除跨进程锁）
│   ├── reducer.rs        # ✅ 保留
│   └── store.rs          # ✅ 保留
├── runtime/              # 删除 session_actor.rs
│   ├── session.rs        # ✅ 保留（直接调用，无 Actor）
│   ├── agent_loop.rs     # ✅ 保留
│   ├── context.rs        # ✅ 保留
│   └── state.rs          # ✅ 删除 pending_inputs
├── executor/             # ✅ 完整保留
│   ├── local.rs
│   ├── process.rs
│   └── filesystem.rs
├── model/                # 只保留 Mock + DeepSeek
│   ├── provider.rs       # ✅ 保留
│   ├── mock.rs           # ✅ 保留
│   └── deepseek.rs       # 简化为单一协议
├── tools/                # ✅ 完整保留
│   ├── registry.rs
│   ├── read.rs
│   ├── edit.rs
│   └── bash.rs
└── policy/               # 简化为 allow/ask 两种
    └── mod.rs
```

**删除的模块**:
- `src/protocol/` 整个目录（-1500 行）→ 独立 crate
- `src/benchmark.rs`（-600 行）→ 独立 crate
- `src/durable/checkpoint.rs`（-1200 行）
- `src/durable/recovery.rs`（-400 行）
- `src/runtime/session_actor.rs`（-1300 行）
- `src/scheduler/`（未实现，可删除）

**总计减少**: ~5000 行（约 30%）

---

### 2.2 简化后的核心概念

初学者只需要理解 **5 个核心概念**：

1. **事件溯源**: 状态 = reduce(事件列表)
2. **Agent Loop**: 模型 → 工具 → 结果 → 循环
3. **工具执行**: 进程管理 + 输出限制
4. **取消与超时**: 如何安全终止运行中的任务
5. **崩溃恢复**: 重放事件日志重建状态

**删除的概念**:
- Checkpoint 优化
- Actor 并发模型
- Durable Queue
- 跨进程并发
- 协议服务器
- Benchmark 系统

---

## 三、关于 "single action" 的问题

你提到的 "single action" 我理解是指"单次工具调用"。当前架构中：

### 3.1 当前实现

`agent_loop.rs:561-914` 的工具执行循环：
```rust
for (call_index, call) in calls.into_iter().enumerate() {
    // 1. append tool.requested
    // 2. lookup tool
    // 3. check policy
    // 4. execute (if approved)
    // 5. append tool.completed
}
```

**问题**: 
- 每个工具都记录 5+ 个事件（requested → started → completed/failed）
- 对于简单场景过于繁琐

### 3.2 简化建议：工具执行合并事件

**当前**（5 个事件）:
```
1. tool.requested
2. tool.approval_requested (if ask)
3. tool.approval_responded (if ask)
4. tool.started
5. tool.completed
```

**简化后**（2 个事件）:
```
1. tool.requested_and_approved   # 合并 1-3
2. tool.completed                 # 合并 4-5
```

**收益**:
- 事件数量减少 60%
- 事件日志更易读
- 仍然保留完整的"请求→结果"追溯链

**代价**:
- 丢失"工具开始执行"和"执行中取消"的精确时间戳
- 对于学习项目，这是可接受的

---

### 3.3 是否进一步简化为"单次 action 不记录事件"？

**不建议**。理由：

1. **丢失崩溃恢复能力**: 
   - 如果工具执行到一半崩溃，重启后无法知道"这个工具是否已经执行过"
   - 可能导致重复执行（例如两次删除同一个文件）

2. **无法调试**:
   - 没有事件记录，出错时无法追溯"模型调用了什么工具"

3. **违背 Event Sourcing 原则**:
   - 状态不再完全由事件决定
   - 引入了"隐式状态"（文件系统副作用）

**结论**: 工具执行必须记录事件，但可以合并为 2 个事件而非 5 个。

---

## 四、推荐的简化路线

### Phase 1: 立即可做（1 周）

1. ✅ **删除 Checkpoint 系统**
   - 移除 `checkpoint.rs` 和 `recovery.rs`
   - 只保留朴素的"从头重放"

2. ✅ **删除 Session Actor**
   - 移除 `session_actor.rs`
   - 只保留 `Session` 直接调用

3. ✅ **删除 Durable Queue**
   - Turn 运行期间拒绝新输入

4. ✅ **简化 Tool Policy**
   - 只保留 `allow` 和 `ask`

**收益**: 减少 ~3000 行，核心逻辑更清晰

---

### Phase 2: 独立模块（2 周）

5. ✅ **协议服务器独立**
   - 新建 `mini-harness-server` crate
   - 移出 `protocol/` 目录

6. ✅ **Benchmark 独立**
   - 新建 `mini-harness-benchmark` crate
   - 移出 `benchmark.rs`

**收益**: 主项目专注核心，高级功能按需引入

---

### Phase 3: 文档优化（1 周）

7. ✅ **README 重写**
   - "5 分钟理解核心架构"
   - "30 分钟从零实现一个 mini-harness"
   - 分阶段教程（而非一次性全讲）

8. ✅ **代码注释增强**
   - 每个核心函数加"为什么需要这样设计"的注释
   - 指出哪些是"简化版"，生产环境需要改进

---

## 五、简化后的教学路径

### 5.1 第一阶段：理解事件溯源（2 小时）

**学习目标**: 理解"状态 = reduce(事件)"

1. 阅读 `event.rs`：30 种事件类型
2. 阅读 `reducer.rs`：如何从事件重建状态
3. 运行 `cargo run -- demo read README.md`
4. 查看生成的 JSONL 日志，手动重放

**代码量**: ~800 行

---

### 5.2 第二阶段：实现 Agent Loop（4 小时）

**学习目标**: 理解模型-工具循环

1. 阅读 `agent_loop.rs` 核心逻辑（删除 checkpoint 后约 600 行）
2. 理解 `continue_turn` 函数：采样 → 工具调用 → 循环
3. 修改 `MockProvider` 实现自定义行为
4. 添加自定义工具

**代码量**: ~1200 行

---

### 5.3 第三阶段：进程管理与取消（3 小时）

**学习目标**: 理解如何安全执行子进程

1. 阅读 `process.rs`：进程组、SIGKILL、输出限制
2. 阅读 `local.rs`：Executor 实现
3. 实验：启动长时间运行的任务并取消

**代码量**: ~800 行

---

### 5.4 第四阶段：崩溃恢复（2 小时）

**学习目标**: 理解如何从崩溃中恢复

1. 阅读 `event_log.rs`：追加写入、fsync
2. 实验：kill -9 强制终止进程
3. 使用 `resume` 命令恢复会话

**代码量**: ~500 行

---

**总学习时间**: 11 小时（而非当前的 20+ 小时）

---

## 六、与主流 Harness 的对比

### 当前 mini-harness（简化前）
- 代码量: 17.7k 行
- 概念数: 12 个
- 学习曲线: 陡峭（需要理解 Actor、Checkpoint、跨进程）

### 简化后 mini-harness
- 代码量: **~8k 行**（减少 55%）
- 概念数: **5 个**（减少 58%）
- 学习曲线: 平缓（专注核心）

### Anthropic Claude Code（生产级）
- 代码量: 估计 50k+ 行
- 概念数: 20+
- 学习曲线: 极陡（不适合学习）

---

## 七、总结建议

### 对于初学者

✅ **应该简化**:
1. 删除 Checkpoint（-1200 行）
2. 删除 Session Actor（-1300 行）
3. 删除 Durable Queue（-300 行）
4. 独立 Protocol Server（-1500 行）
5. 独立 Benchmark（-600 行）

✅ **不应该简化**:
1. Event Sourcing 核心（这是项目灵魂）
2. Agent Loop（这是核心逻辑）
3. 工具执行（这是实际问题）
4. 进程管理（这是工程必备）

### 关于 "single action"

- ✅ **合并工具事件**：5 个 → 2 个（合理）
- ❌ **不记录工具事件**：违背 Event Sourcing（不推荐）

### 下一步

1. 你是否同意这个简化方向？
2. 是否需要我帮你实施 Phase 1（删除 Checkpoint + Actor）？
3. 对于"single action"，是采用"合并事件"还是有其他想法？
