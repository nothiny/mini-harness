# mini-harness 简历文案 - 极简精炼版

---

## 版本 1: 最精简版（适合空间极度受限）

### mini-harness | Agent Harness 运行时 | Rust

从零实现 Agent Harness 核心能力，理解"模型循环之外的全部系统"

**Event Sourcing 状态管理** - JSONL 事件日志作为唯一事实来源，确定性 Reducer 从事件重建状态，支持崩溃恢复

**Agent Loop 引擎** - 模型采样 → 工具调用 → 结果返回循环，支持 max_steps/timeout/中途取消

**工具执行管控** - 三级授权策略（allow/ask/deny），workspace 边界检查防止路径逃逸

**进程生命周期** - 进程组级取消清理子进程树，三层输出限制（per-tool/累计/UTF-8截断）防止 OOM

**Provider 抽象** - 统一接口支持 OpenAI/DeepSeek，MockProvider 支持离线开发

**技术栈**: Rust · Tokio · Event Sourcing  
**成果**: ~8k 行，完整测试覆盖，架构审查报告

---

## 版本 2: 更精简版（每条只保留核心点）

### mini-harness | Agent Harness 运行时 | Rust

从零实现 Agent 基础设施核心能力

- **Event Sourcing**: JSONL 日志 + 确定性 Reducer，支持崩溃恢复和时间旅行调试
- **Agent Loop**: 模型-工具循环，支持 max_steps/timeout/中途取消
- **工具管控**: 三级授权策略 + workspace 边界检查
- **进程管理**: 进程组级取消清理子进程树，三层输出限制防止 OOM
- **Provider 抽象**: 统一接口支持多模型，离线 Mock 测试

**技术栈**: Rust · Tokio · Event Sourcing | **代码**: ~8k 行

---

## 版本 3: 极简一句话版（适合项目列表）

### mini-harness | Agent Harness 运行时 | Rust

从零实现 Agent 基础设施：Event Sourcing 状态管理、Agent Loop 执行引擎、工具授权与进程管控、Provider 抽象。Rust · Tokio · ~8k 行

---

## 版本 4: 核心能力突出版（推荐）

### mini-harness | Agent Harness 运行时 | Rust

从零实现 Agent Harness 核心能力，深入理解"模型循环之外的全部系统"

**状态管理** - Event Sourcing 架构，JSONL 日志作为唯一事实来源，确定性 Reducer 保证重放一致性，支持崩溃恢复

**执行控制** - Agent Loop 实现模型-工具循环，三级工具授权策略（allow/ask/deny），workspace 边界检查防止路径逃逸

**进程管控** - 进程组级取消（kill -pgid）清理子进程树，三层输出限制（per-tool/累计/UTF-8安全截断）防止 OOM

**Provider 抽象** - ModelProvider trait 统一接口支持 OpenAI/DeepSeek，MockProvider 实现离线开发

**技术栈**: Rust · Tokio · Event Sourcing | **代码**: ~8k 行，完整测试，架构审查文档

---

## 版本 5: 问题导向版（展示理解深度）

### mini-harness | Agent Harness 运行时 | Rust

从零实现 Agent 基础设施，理解并解决 Harness 核心挑战

**挑战 1: 状态如何在不可靠环境中保持一致？**
→ Event Sourcing：JSONL 日志 + 确定性 Reducer，重放即可恢复

**挑战 2: 工具如何安全执行且可控制？**
→ 三级授权策略、workspace 边界检查、进程组级取消、三层输出限制

**挑战 3: 模型-工具循环如何推进？**
→ Agent Loop：sample → tool calls → results，支持 max_steps/timeout/cancel

**挑战 4: 如何抽象不同 Provider？**
→ ModelProvider trait 统一接口，MockProvider 离线测试

**技术栈**: Rust · Tokio · Event Sourcing | **代码**: ~8k 行

---

## 面试准备 - 核心话术（30 秒版）

"我从零实现了一个 Agent Harness，约 8k 行 Rust 代码。

核心解决了四个问题：
1. **状态一致性** - Event Sourcing 保证崩溃可恢复
2. **工具安全** - 三级授权 + 进程组取消 + 输出限制
3. **循环控制** - Agent Loop 的执行与中途取消
4. **Provider 抽象** - 统一接口支持多模型

关键设计是 Event Sourcing：所有状态变化都记录为事件，重放就能恢复。我用进程组取消清理子进程树，用三层输出限制防止 OOM，用确定性 Reducer 保证重放一致性。

完成后做了架构审查，识别了 12 个问题并给出改进方案，展示了对'简化版'和'生产级'差距的理解。"

---

## 面试深度问答（精选 5 个必答）

### Q1: 为什么选择 Event Sourcing？

**30 秒版本**:
"Agent 系统的核心挑战是**在不可靠环境中保证状态一致性**。传统的内存状态 + 定期 checkpoint，进程 kill -9 后会丢失状态。Event Sourcing 把每个状态变化都记录为事件，重放就能完整恢复。而且所有决策（工具调用、用户审批）都有记录，天然支持审计和调试。"

**深入版本**（如果追问）:
"确定性 Reducer 是核心：相同事件序列 → 相同状态，无论重放多少次。这要求不能依赖 `now()`/`random()`，时间戳必须来自事件本身。我用 Property-Based Testing 验证了这个不变量：生成 24 种随机事件序列，每个都重放两次，状态必然相同。"

---

### Q2: 进程组取消解决了什么问题？

**30 秒版本**:
"传统的 `kill(pid)` 只杀父进程，bash 启动的子进程会变成孤儿进程泄漏。例如 `bash 'nohup sleep 1000 &'`，kill bash 后 sleep 仍在后台运行。

我用进程组级取消：创建进程时 `setpgid(0, 0)` 新建进程组，取消时 `kill(-pgid, SIGKILL)` 一次性清理整个树。负号表示进程组 ID。"

---

### Q3: 三层输出限制是什么？

**30 秒版本**:
"单一限制不够：一个工具耗尽配额，后续工具就没输出了。我设计了三层：

1. **per-tool**: 每个工具最多 200KB
2. **cumulative**: 单个 turn 累计最多 2MB（跨工具共享）
3. **UTF-8 安全**: 不能从多字节字符中间截断

Cumulative limit 用 `AtomicUsize` + CAS 实现并发分配，多个工具同时执行时竞争配额。"

---

### Q4: 中途取消如何保证正确性？

**30 秒版本**:
"取消本身也是一个 durable 事件，不是内存 flag。

流程是：用户 cancel → 写入 `CancelRequested` 事件 → 运行中的进程下次追加事件前检测到日志变化 → 读取发现取消 → 主动停止。

这是 Fail-Stop 语义：发现冲突时拒绝追加，而非覆盖别人的写入。支持跨进程取消：进程 A cancel、进程 B 正在运行，进程 B 会正确检测到并停止。"

---

### Q5: 你的项目与生产级 Harness 的差距？

**诚实回答**（展示认知深度）:
"我完成后做了架构审查，识别了差距：

**简化版** vs **生产级**:
- 工具串行执行 vs 并行 + 依赖图
- 简单 sliding window vs 智能 Context 管理
- 路径级边界检查 vs OS 级沙箱（Landlock）
- 无 Streaming vs 实时响应流

但核心能力一致：Event Sourcing、Agent Loop、工具授权、进程管理、Provider 抽象。

我给出了三阶段改进路线，估算了每个任务的成本。这展示了我不只是'写完代码'，而是**理解为什么生产级要这样设计**。"

---

## 最终推荐

**简历中使用**: **版本 4（核心能力突出版）**
- 4 个核心模块（状态/执行/进程/Provider）
- 每个模块 1-2 句话讲清楚做了什么
- 技术栈 + 代码量简洁收尾
- 总字数约 120 字（中文），适合简历一个项目条目

**面试准备**: 
- 30 秒核心话术（开场介绍）
- 5 个必答问题（每个 30 秒版本 + 深入版本）
- 重点强调：Event Sourcing、进程组取消、三层限制、Durable Cancel、架构自审

**核心策略**:
- 简历：精炼到极致，每个字都有信息量
- 面试：展开细节，用具体技术点证明深度
- 心态：不是"写了个玩具"，而是"理解了本质问题"
