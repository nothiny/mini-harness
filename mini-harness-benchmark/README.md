# mini-harness-benchmark

独立的 Benchmark 运行器：以固定、非交互的方式运行任务清单并输出
机器可读结果。所有工具权限强制为 `allow`，一次调用绝不会等待人工。

本工具只负责编排（逐任务启动 harness turn、保留证据），不运行验证器、
不了解具体的评测套件。外层运行器（例如 Harbor）准备 workspace、调用
本工具，再用套件自己的验证器评估结果。

## 使用

```bash
cargo run -p mini-harness-benchmark -- tasks/tasks.example.toml \
  --provider deepseek --model deepseek-chat \
  --artifacts ./artifacts --results ./results.json
```

## 输出

stdout 与 `--results` 均收到同一份 JSON 汇总:

```json
{
  "manifest": "tasks.example.toml",
  "finished": true,
  "total_tasks": 2,
  "completed": 2,
  "failed": 0,
  "results": [
    {
      "task_id": "hello",
      "execution_status": "succeeded",
      "event_log": "artifacts/run-.../0000-hello.jsonl"
    }
  ]
}
```

## Harbor 集成

参见 `tasks/harbor/README.md` 了解如何在容器化环境中运行
Terminal-Bench 2.1 任务。
