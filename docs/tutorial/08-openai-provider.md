# 第 8 章：真实 provider——Responses 与 Chat Completions

前面一直用 MockProvider，是为了先把 runtime 语义固定下来。这一章看真实 HTTP provider 怎样适配同一套 ModelProvider 接口。

当前项目的 provider 代码同时支持：

- OpenAI Responses wire format；
- DeepSeek、Kimi、Qwen、vLLM、Ollama 等兼容的 Chat Completions wire format。

教程先讲共同的数据流，再看两个协议的差异。

## 先看三层代码

~~~text
src/model/provider.rs
  ModelRequest / ModelResponse / ModelCompletion

src/model/openai_protocol.rs
  纯函数：runtime 类型 ↔ JSON body / JSON response / SSE

src/model/openai.rs
  配置、HTTP 请求、超时、取消、重试、attempt 观察
~~~

runtime 只依赖 ModelProvider。它不知道 endpoint 的 JSON 字段，也不知道 function_call_output 放在 input 还是 message 里。

这就是 provider 可替换的实际含义：只要返回相同的 ModelResponse 和 ModelCompletion，AgentLoop 就能继续执行。

## 两种 wire format 的共同闭环

工具调用会形成两次模型请求：

~~~text
请求 1：用户历史 + tools
响应 1：工具调用

runtime 执行工具
  └─ 产生 ToolResult

请求 2：原来的对话 + 工具结果 + continuation
响应 2：最终文本或下一批工具调用
~~~

Responses 可能使用 previous_response_id 和 function_call_output；Chat Completions 可能把 assistant tool_calls 和 tool message 放在 messages 中。字段不同，但 runtime 要的行为相同。

## ProviderContinuation 是翻译桥

打开 src/runtime/state.rs。ProviderContinuation 中的字段大致包括：

~~~text
provider
response_id
model
endpoint
native_call_ids: runtime ToolCallId → provider call id
history_cursor
reasoning_content（兼容思考模型）
~~~

runtime 的 ToolCallId 属于自己的命名空间。provider 的 call_id 属于 provider。native_call_ids 负责把两边映射起来。

continuation 会作为 provider.continuation.updated 事件持久化。重启后，provider 还能知道当前响应链走到哪里；如果 model 或 endpoint 变化，provider 会回放规范化历史，而不引用旧 response_id。

history_cursor 表示续传需要的历史位置。它让 provider 能发送增量上下文，也让实现明确知道“已经交给 provider 的历史从哪里结束”。

## 读请求构造函数

打开 src/model/openai_protocol.rs，先看纯函数而不是 HTTP：

- build_request_body：Responses body；
- build_cc_request_body：Chat Completions body；
- parse_json_response 和 parse_cc_json_response：解析文本与工具调用；
- parse_stream_response 和 parse_cc_stream_response：聚合 SSE 增量。

纯函数的优点是：你可以用固定 JSON fixture 检查协议转换，不需要网络，也不需要 API key。

解析器需要逐字段校验：

~~~text
响应 id 是否存在
output 或 choices 是否是预期数组
文本与工具调用是否能互相区分
call_id、name、arguments 是否完整
usage 是否能安全解析
~~~

schema 不合法是 provider 实现或服务响应问题，不应该靠无限重试掩盖。

## 配置和安全边界

打开 src/model/openai.rs 的 OpenAiProviderConfig 和 WireStyle。配置阶段会检查：

- model 不为空；
- max_retries 在合理范围；
- endpoint 只允许可信 URL；
- 公共地址使用 HTTPS；
- API key 不进入 Debug、事件或协议；
- 响应体和单次请求有大小、时间限制。

API key 只在 provider 内部使用。日志里可以记录 request id、response id、attempt 号和 usage，但不能记录密钥或完整错误 body。

## 重试怎样和事件结合

provider 可能遇到网络抖动、408、429 或 5xx。当前实现会区分可重试和不可重试：

~~~text
网络抖动 / 408 / 429 / 5xx
  → 有限次数重试，必要时读取 Retry-After

400 / 401 / 响应 schema 错
  → 记录失败并返回
~~~

每次尝试都可以通过 ProviderObserver 记录 ProviderAttempt。Session actor 只有在 attempt 事件追加成功后才确认给 provider，保证“日志里看到的尝试”与 runtime 观察一致。

## 取消从哪里进入

ModelProvider::complete 接收 CancellationToken。OpenAiProvider 在 HTTP 请求等待期间 select 取消信号；取消后立即返回 provider 错误或取消错误，AgentLoop 再根据 turn 的原因追加终态事件。

这条路径和 MockProvider 的 Delay 脚本共享同一行为契约。先在 mock 中验证取消，再看真实 HTTP client 如何实现。

## 离线测试怎么读

~~~bash
cargo test --test openai_provider
cargo test --test provider_contract
~~~

这些测试用 tokio TcpListener 启动本地 mock server。读测试时先找 server 收到的 request body，再找 provider 解析的 ModelResponse，最后看 AgentLoop 是否按同一语义继续。

契约测试的价值在于：MockProvider 和 OpenAiProvider 不是靠“接口看起来一样”证明可替换，而是要通过相同的文本、工具调用、错误、取消和续传场景。

## 思考题

1. 为什么 runtime 不直接保存 provider 的 native call_id？
2. 如果换 model 后旧 response_id 失效，provider 应该怎样构造下一次请求？
3. 429 的 Retry-After 为什么比固定 sleep 更可靠？
4. 响应 JSON 不合法时为什么不建议直接重试？
5. API key 只出现在 provider 内部，为什么仍然要测试 Debug 输出和事件日志？
6. “请求已发出但进程在收到响应前崩溃”属于 provider 失败、结果未知，还是另一种事实？你会把它放在哪一层处理？

下一章：[CLI 和 JSONL 协议——把 runtime 放到进程边界外](09-cli-and-protocol.md)。

