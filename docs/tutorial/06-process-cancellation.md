# 第 6 章：进程、取消和原子编辑

前一章说明 AgentLoop 如何启动工具。这一章进入最容易出真实事故的地方：操作系统。

bash 会启动进程，进程会产生输出，也可能再启动子进程。edit 会改用户文件，可能和另一个进程同时修改同一个路径。当前项目把这些副作用集中在 executor 层，并用 cancellation、timeout、输出上限和 hash guard 保护它们。

## 从 BashTool 追到 process executor

按这个顺序读源码：

~~~text
src/tools/bash.rs
  └─ 解析 command、timeout_ms、输出预算
      ▼
src/executor/trait.rs
  └─ ProcessRequest
      ▼
src/executor/local.rs
  └─ 委托给 run_process
      ▼
src/executor/process.rs
  └─ spawn、读管道、取消、回收和汇总结果
~~~

BashTool 面向模型，负责输入格式和 JSON 结果。ProcessRequest 面向 executor，负责时间和字节预算。process.rs 才直接处理 Child、stdout、stderr 和进程组。

## 为什么 stdout 和 stderr 必须并发读取

子进程的 stdout 和 stderr 都是有容量的管道。如果父进程只读 stdout，子进程持续写满 stderr，就会卡在写 stderr 上，父进程等待它退出也会一直等。

当前实现为两条流分别启动排空任务：

~~~text
child.stdout ──▶ stdout reader ──▶ 保留上限内的文本
child.stderr ──▶ stderr reader ──▶ 保留上限内的文本
                         │
                         └─ 超过上限仍继续读取，只是不再保存
~~~

“停止保留”和“停止读取”不是一回事。停止读取会让子进程被管道反压，最终造成死锁。

## 三层输出上限

ProcessRequest 包含：

- max_stdout_bytes；
- max_stderr_bytes；
- max_combined_bytes。

单独的 stdout 和 stderr 上限防止某条流占满内存；combined 上限防止两条流各自没有超限、合计却绕过限制。

ProcessOutput 会告诉模型：

~~~text
text
truncated
original_bytes
retained_bytes
~~~

这样模型知道收到的是完整结果，还是被截断的结果。UTF-8 截断必须退回到字符边界，不能直接在任意字节位置构造 String。

## 进程组和子孙进程

只杀掉 shell 的父进程不够。命令可能启动 sleep、编译器或后台子进程。Unix 平台上当前实现把命令放进独立进程组；取消或超时时按进程组清理。

ProcessGroupGuard 的思路是：

~~~text
启动后 armed
  └─ 任何提前返回都尝试杀进程组
正常 wait 并完成收尾
  └─ disarm
~~~

Drop 是最后一道保险。测试里常见的 sleep 与后台子进程场景，就是为了验证“取消后没有孤儿进程”。

## 取消、超时和结果未知

AgentLoop 给每个 turn 一个 CancellationToken，工具可以使用它，进程执行器再把取消翻译成杀进程组。

三个时间概念要区分：

- turn wall clock：整个 turn 的墙钟预算；
- cumulative tool time：所有工具累计执行时间；
- process timeout：单个 bash 命令的时间预算。

用户取消记录 turn.cancel_requested 和 turn.cancelled。时间预算耗尽记录 turn.timed_out。

如果工具开始执行后，进程被杀或清理超时，runtime 可能无法知道外部副作用是否完成。这时记录 tool.outcome_unknown。它和 tool.failed 的差异是：

~~~text
Failed
  → 可以确定工具没有完成预期操作

OutcomeUnknown
  → 本地日志无法证明外部操作是否发生
~~~

第 7 章会把这个状态交给恢复系统处理。

## edit 为什么要 hash guard

打开 src/executor/filesystem.rs 和 src/tools/edit.rs。一次编辑大致是：

~~~text
解析 workspace 路径
  → 获取 advisory lock
  → 读取当前文件
  → 计算 old_hash
  → 校验 expected_hash（如果提供）
  → 要求 old_text 恰好匹配一次
  → 写同目录临时文件
  → flush / fsync
  → rename 原子替换
  → 返回 old_hash、new_hash、replacements
~~~

hash guard 防止模型基于旧内容做编辑，把用户刚才的修改覆盖掉。精确匹配防止“替换错位置”或“同时替换多个位置”。

临时文件和 rename 解决的是崩溃完整性：进程在写入期间崩溃，原文件仍然存在；只有完整的新文件准备好后才替换。

取消检查放在编辑事务边界上。rename 事务内部不能被随意劈开，否则就可能留下半个写入。

## 运行哪些测试

还可以观察一个离线的 edit → bash 闭环：

~~~bash
cargo run -- demo edit-bash README.md
~~~

demo 使用 AllowAllPolicy 只是为了演示完整链路；真实会话默认仍由 DefaultPolicy 决定是否需要审批。

~~~bash
cargo test --test process_executor
cargo test --test bash_tool
cargo test --test edit_tool
cargo test --test session_cancel
cargo test cross_process_cancel
~~~

先读大输出测试，再读取消测试，最后读 edit 的 hash conflict 测试。每个测试都问“它在防止哪个现实中的事故”。

## 思考题

1. 为什么达到 stdout 上限后仍要继续读取管道？
2. kill 父进程后，为什么子进程可能继续存在？进程组解决了哪一层关系？
3. 非零 exit code 为什么通常是模型可以处理的结果，而无法回收进程是 executor 错误？
4. 取消一个 edit 时，为什么要把取消检查放在 rename 前，而不是每写一行都检查？
5. read 的结果未知通常可以重试，bash 的结果未知为什么默认要人工决定？
6. 如果两个 harness 实例同时编辑一个文件，advisory lock 和 hash guard 各自解决什么问题？
7. 输出上限、历史上限和事件大小上限分别保护哪个边界？

下一章：[崩溃恢复——系统怎样面对“不知道”](07-recovery.md)。
