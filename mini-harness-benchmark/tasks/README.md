# Benchmark adapters

This directory contains optional benchmark integrations. They are not part of
the normal Rust or TUI build and do not add a runtime dependency on Harbor.

The first integration is a Harbor `BaseInstalledAgent` adapter for
`mini-harness`: [harbor/README.md](harbor/README.md).

## Local manifest runner

The Rust binary also provides a small non-interactive runner for fixed
development subsets. A manifest is TOML or JSON and names one task workspace
per task:

```toml
name = "terminal-bench-2.1-cpu-dev"

[[tasks]]
id = "example/task"
prompt = "Complete the task instructions. Run the relevant checks before finishing."
workspace = "/tmp/benchmark-task"
metadata = { suite = "terminal-bench", release = "2.1" }
```

Run it with:

```bash
mini-harness-benchmark/target/debug/mini-harness-benchmark benchmark.toml \
  --provider deepseek --model deepseek-chat \
  --artifacts ./benchmark-artifacts \
  --results ./benchmark-results.json
```

The runner forces `read_workspace`, `edit_workspace`, and `bash` to `allow`
inside the task attempt so it never waits for a human. This runner does not
install an OS-level shell sandbox; use Harbor's task container for the outer
execution boundary. Each attempt gets a fresh artifact directory containing
the event log, checkpoint, preserved manifest, and redacted effective config.
The incremental `results.json` records `execution_status`,
`failure_category`, budget settings, and `task_result: "not_verified"`; it does
not claim task success. Harbor or another benchmark verifier must inspect the
resulting workspace and set the task outcome.

For the pinned Terminal-Bench 2.1 development subset, use
[`harbor/terminal-bench-2.1-dev.yaml`](harbor/terminal-bench-2.1-dev.yaml)
and its [run instructions](harbor/README.md#fixed-terminal-bench-21-development-subset).

Pure command/config helpers can be tested without installing Harbor:

```bash
python3 -m unittest discover -s mini-harness-benchmark/tasks/harbor -p 'test_*.py'
```
