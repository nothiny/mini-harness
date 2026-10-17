# mini-harness 简历文案 - 最终精简版

---

## 简历版本（直接可用）

### mini-harness | Agent Harness 运行时 | Rust

从零实现 Agent Harness 核心能力，深入理解"模型循环之外的全部系统"

**Event Sourcing 状态管理**
- JSONL 事件日志作为唯一事实来源，确定性 Reducer 从事件重建状态，支持崩溃恢复和时间旅行调试

**Agent Loop 执行引擎**
- 实现 sample → tool calls → results 循环，支持 max_steps 限制、turn 超时和中途取消
- Context 管理：sliding window 策略，provider cursor 避免重复发送缓存响应

**工具执行与边界控制**
- 三级授权策略（allow/ask/deny），危险工具需用户审批
- Workspace 边界检查（路径规范化 + 符号链接防逃逸），完整生命周期跟踪

**进程生命周期管理**
- 进程组级取消（kill -pgid）清理子进程树，防止僵尸进程
- 三层输出限制（per-tool / 累计 / UTF-8 安全截断）防止 OOM

**技术栈**: Rust · Tokio · Event Sourcing  
**代码**: ~8k 行，完整测试覆盖

---

## 面试 30 秒开场

"我从零实现了一个 Agent Harness，约 8k 行 Rust 代码。

Harness 是'模型循环之外的全部系统'，核心解决四个问题：

1. **状态一致性** - Event Sourcing：所有变化记录为事件，重放即可恢复
2. **工具安全** - 三级授权策略 + workspace 边界检查
3. **循环控制** - Agent Loop 执行 + 中途取消机制
4. **进程管控** - 进程组级取消清理子进程树，三层输出限制防止 OOM

关键设计是 Event Sourcing：确定性 Reducer 保证重放两次必然相同状态。"

---

## 面试核心问答（必备 3 个）

### Q1: 为什么选择 Event Sourcing？

"Agent 系统的核心挑战是**在不可靠环境中保证状态一致性**。

传统方案：内存状态 + 定期 checkpoint → kill -9 后丢失状态

Event Sourcing：所有变化记录为事件 → 重放即可完整恢复

而且所有决策（工具调用、用户审批）都有记录，天然支持审计和调试。

核心是确定性 Reducer：相同事件序列 → 相同状态，不依赖 `now()`/`random()`。"

---

### Q2: 进程组取消解决了什么问题？

"传统 `kill(pid)` 只杀父进程，子进程会泄漏。

例如 `bash 'nohup sleep 1000 &'`，kill bash 后 sleep 仍在后台运行。

我的解决：
1. 创建进程时 `setpgid(0, 0)` 新建进程组
2. 取消时 `kill(-pgid, SIGKILL)` 一次性清理整个树

负号表示进程组 ID，能清理所有子孙进程。"

---

### Q3: 三层输出限制是什么？

"单一限制不够：一个工具耗尽配额，后续工具就没输出了。

我设计了三层：
1. **per-tool**: 每个工具最多 200KB
2. **cumulative**: 单个 turn 累计最多 2MB（多工具共享）
3. **UTF-8 安全**: 不能从多字节字符中间截断

Cumulative 用 `AtomicUsize` + CAS 实现并发分配，多个工具同时执行时竞争配额。"

---

## 如果追问细节

### Q: Event Sourcing 的"确定性"如何保证？

"不能依赖外部输入：
- ❌ `Instant::now()` - 每次重放时间不同
- ❌ `rand::random()` - 随机数不可重现
- ✅ 时间戳来自事件本身的 `timestamp` 字段
- ✅ 随机性来自模型响应（已记录在事件中）

这样重放多次，状态必然相同。"

---

### Q: 中途取消如何实现？

"取消本身也是一个 durable 事件，不是内存 flag。

流程：
1. 用户 cancel → 写入 `CancelRequested` 事件
2. 运行中的进程下次追加事件前检测到日志变化
3. 读取日志发现取消 → 主动停止

这是 Fail-Stop 语义：发现冲突时拒绝追加，而非覆盖。

支持跨进程取消：进程 A cancel、进程 B 运行，进程 B 正确检测并停止。"

---

### Q: 与生产级 Harness 的差距？

"我做了架构审查，识别了差距：

**简化版** vs **生产级**:
- 工具串行 vs 并行 + 依赖图
- 简单 sliding window vs 智能 Context 管理
- 路径检查 vs OS 级沙箱（Landlock）
- 无 Streaming vs 实时响应流

但核心能力一致：Event Sourcing、Agent Loop、工具授权、进程管理。

我给出了三阶段改进路线，估算了每个任务的成本。这展示了我理解'为什么生产级要这样设计'。"

---

## 核心优势总结

✅ **完整性** - 不是玩具，覆盖 Harness 核心能力  
✅ **深度** - Event Sourcing、进程组管理、确定性 Reducer 都有实际实现  
✅ **反思** - 架构审查识别问题 + 改进方案  
✅ **对标** - 知道与生产级的差距在哪

**关键话术**: "从零实现"、"理解本质问题"、"确定性 Reducer"、"进程组级取消"、"三层输出限制"
