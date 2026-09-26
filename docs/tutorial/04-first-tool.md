# 第 4 章：Tool、Policy 和 Executor——一次 read 怎样到达文件系统

这一章回答：模型提出了一个工具调用后，谁验证参数，谁决定权限，谁真正碰文件系统？

当前项目刻意把这三个角色拆开。读懂 read 以后，edit 和 bash 就有了共同的框架。

## 一次 read 的完整路径

以 demo 中的 read 为例：

~~~text
ModelResponse::ToolCall
  │ name = "read", input = {"path":"README.md"}
  ▼
ToolRegistry::lookup("read")
  ▼
ToolPolicy::decide(...)
  ▼
ReadTool::execute(input, ToolContext)
  ▼
Executor::read_file(ReadFileRequest)
  ▼
LocalExecutor 的文件访问
  ▼
ReadTool 把结果包装成模型可读 JSON
~~~

四层分别回答不同问题：

| 层 | 主要问题 | 当前文件 |
|---|---|---|
| Tool | 参数表示什么，结果怎样给模型 | src/tools/read.rs |
| Registry | 这个名字是否注册，spec 是什么 | src/tools/registry.rs |
| Policy | 这次调用能不能开始 | src/policy/mod.rs |
| Executor | 怎样实际访问文件或进程 | src/executor/ |

如果把四层合成一个函数，任何一层的职责都会变得含糊：工具可能绕过权限，policy 可能开始执行进程，executor 可能开始理解模型消息。

## Tool trait 和 ToolContext

打开 src/tools/spec.rs。Tool trait 有两个核心方法：

~~~text
spec() -> ToolSpec
execute(input, ToolContext) -> Result<String, String>
~~~

ToolSpec 是给模型看的描述：

- name：模型要填的工具名；
- description：模型要理解的用途；
- parameters：JSON Schema；
- risk：Read、Write 或 Execute。

ToolContext 是这次调用获得的最小能力集：

~~~text
executor
cancel
workspace
tool_call_id
execution_id
output_limit
~~~

工具拿不到 SessionState、EventStore 或 SessionHandle。它只能通过 executor 做被允许的系统操作，然后返回一个模型可读字符串。执行器仍然使用类型化错误；只有在 Tool 这一层，错误才被序列化成 JSON 字符串。

## ReadTool 具体做了什么

打开 src/tools/read.rs 的 execute，按顺序读：

1. parse_input：确认输入是对象；
2. 拒绝未知字段；
3. 确认 path 是非空字符串；
4. 检查 start_line 和 end_line 是从 1 开始的整数；
5. 通过取消令牌调用 executor；
6. 把字节结果切成行窗口；
7. 限制最多 200 行；
8. 返回文本和截断信息。

这个顺序说明了两种不同的保护：

- 参数保护：防止模型传入无法解释的 JSON；
- 资源保护：防止一个合法文件把模型上下文撑爆。

LocalExecutor 还会处理目录、不存在、权限、二进制和 workspace 外路径等系统错误。ReadTool 不需要知道这些错误如何从操作系统产生，只负责把它们转换成模型能理解的结构化结果。

## Policy 是执行前的门

打开 src/policy/mod.rs。DefaultPolicy 的决定依赖 ToolSpec 的风险等级和这次 input：

~~~text
Read
  workspace 内 → Allow
  workspace 外或没有 workspace → Deny

Write
  workspace 内 → Ask
  workspace 外 → Deny

Execute
  被识别为只读命令 → Allow
  其他命令 → Ask
~~~

Allow、Ask、Deny 都只是决定，不是执行结果：

- Allow：runtime 追加 tool.started，然后调用工具；
- Ask：runtime 追加 tool.approval.requested，等待用户；
- Deny：runtime 追加 tool.policy_denied，不启动工具。

当前 DefaultPolicy 会识别一部分只读 shell 命令，例如 ls、cat、grep 和 pwd；遇到重定向、命令替换、串联等可能产生副作用的写法，会要求审批。

Policy 应该 fail closed：缺少 workspace、缺少 path 或无法判断风险时，不能因为“看起来可能没事”就允许。

## workspace 边界要看两次

路径安全不是简单地把 root 和 path 拼起来。当前实现至少要考虑：

~~~text
notes.txt                 workspace 内
../outside.txt            词法逃逸
/absolute/path            绝对路径
symlink → /etc/passwd     解析后逃逸
~~~

LocalExecutor 会做词法和规范化检查；更底层的安全文件操作还在 executor/filesystem_secure.rs 中针对不同操作系统处理。Linux 上可以使用 openat2 等能力缩小检查与打开之间的窗口，其他平台有不同的保护程度。

读实现时不要把“拒绝明显的 ../”误读成“完全解决 TOCTOU”。安全边界通常是多层防线，文档和代码要明确每层解决什么。

## Registry 解决模型可见性

src/tools/registry.rs 的 ToolRegistry 使用 BTreeMap 保存工具：

- 空名称拒绝；
- 重复名称拒绝；
- 序列化后的 spec 超过 16 KiB 拒绝；
- specs 返回稳定顺序；
- lookup 未知名称返回结构化错误。

稳定顺序让相同的工具集合生成相同的 ModelRequest，也让测试和诊断更容易比较。

## 用 demo 和测试观察这条链

运行：

~~~bash
cargo run -- demo read README.md --json
cargo test --test executor_read
cargo test --test read_tool
cargo test --test policy
~~~

读 JSON 时注意：

- ToolRequested 保存完整调用输入；
- ToolStarted 才表示真的进入执行；
- ToolCompleted 才表示 executor 返回了结果；
- Policy 拒绝不会出现 ToolStarted。

你可以把 path 改成 ../outside，或请求一个不存在文件，比较“策略拒绝”和“执行器报 NotFound”的差异。两者发生的时间和责任人不同。

## 思考题

1. 为什么 ReadTool 不直接接收 LocalExecutor，而是接收 Executor trait？
2. workspace 外的 read 为什么直接 Deny，而不是 Ask？
3. 一个 path 在词法上位于 workspace 内，但通过 symlink 指向外部，哪一层应该拒绝？
4. Tool 返回字符串错误，为什么 Executor 仍然保留类型化错误？
5. ToolRegistry 的 spec 大小上限保护的是哪一个资源？
6. 如果你要加入一个列出环境变量的工具，它的 RiskClass 应该是什么，DefaultPolicy 是否足够？

下一章：[AgentLoop——一轮 turn 怎样反复采样](05-agent-loop.md)。

