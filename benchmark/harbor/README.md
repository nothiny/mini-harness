# Harbor adapter

`MiniHarnessAgent` is a thin Harbor `BaseInstalledAgent` adapter. Harbor still
creates the task environment and runs the verifier; the adapter only starts the
Rust binary and preserves its native JSONL event log.

The adapter targets the current Harbor custom-agent interface described in the [official Harbor agent docs](https://github.com/harbor-framework/harbor/blob/main/docs/content/docs/agents/index.mdx):

```text
harbor.agents.installed.base.BaseInstalledAgent
  install(environment)
  run(instruction, environment, context)
```

Harbor is optional for this repository. Installing or importing this module is
only needed for benchmark runs. The adapter is pinned to Harbor `0.23.0`, whose
released package requires Python `>=3.12`:

```bash
uv tool install harbor==0.23.0
# or: python3.12 -m pip install -r benchmark/harbor/requirements.txt
```

The fake-environment contract tests do not require Harbor and run on the
repository's normal Python installation.

## Build the binary

Build a Linux binary that can run inside the task image. A statically linked
release binary is easiest to transfer between images:

```bash
cargo build --release
```

The adapter accepts either a binary already available in the task image, or a
host path that Harbor uploads during setup.

## Run one task

From the repository root, with Harbor installed:

```bash
harbor run \
  --path /path/to/task \
  --agent-import-path benchmark.harbor.mini_harness_agent:MiniHarnessAgent \
  --model deepseek/deepseek-chat \
  --ak binary=/absolute/path/to/target/release/mini-harness \
  --ae DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY"
```

Use `--ak binary=mini-harness` when the task image already contains the
binary. Harbor's `--agent-env` (`--ae`) is the supported way to pass provider
credentials. The adapter explicitly passes the provider API key into the
harness command. Harbor still controls the trial's scoped environment overlay;
unrelated host secrets are not copied by this adapter. The current Rust
provider uses fixed OpenAI/DeepSeek endpoints, so `*_BASE_URL` values are not
supported benchmark settings.

For a local smoke test with a bare model name, provide the provider explicitly:

```bash
harbor run ... \
  --agent-import-path benchmark.harbor.mini_harness_agent:MiniHarnessAgent \
  --model deepseek-chat \
  --ak provider=deepseek
```

The trial config automatically allows `read_workspace`, `edit_workspace`, and
`bash` because Harbor tasks have no interactive approval channel. This only
changes the policy inside the isolated task environment. The adapter leaves
network policy to Harbor's task environment; an
`offline_strict` mini-harness setting is rejected until the sandbox network
backend is present in the binary. This prevents a benchmark run from claiming
network isolation that it did not actually install.

By default, the adapter uses Harbor's task `workdir` as the mini-harness
workspace root. Terminal-Bench tasks are different: several tasks intentionally
modify system paths such as `/etc`, `/var`, or `/usr`. The checked-in
Terminal-Bench configuration therefore sets `workspace_root: /`, which makes
the harness and its path policy cover the whole task container. Harbor creates a
fresh isolated container for each trial, so this setting does not expose the
host filesystem. Keep the default task workdir for adapters running in a shared
or non-isolated environment.

## Evidence and result interpretation

Each trial leaves these files under Harbor's agent log directory:

- `mini-harness-events.jsonl`: append-only native event log;
- `mini-harness.stdout`: command output, including the final JSON result;
- `mini-harness-metadata.json`: provider, model, workspace, network, and budget;
- `mini-harness-execution.json`: whether the harness command exited successfully
  and its exit code, including early failures.

Harbor's verifier is the task result. A successful process exit or a final
assistant message is not treated as a task success. Keep the Harbor reward and
the mini-harness event log together when comparing sandbox configurations.

The adapter deliberately does not implement a second Python agent loop, parse
assistant text into a patch, or decide whether a task passed. Those decisions
belong to mini-harness and Harbor's verifier respectively.

## Fixed Terminal-Bench 2.1 development subset

The checked-in [`terminal-bench-2.1-dev.yaml`](terminal-bench-2.1-dev.yaml)
pins the Terminal-Bench 2.1 repository commit and selects eleven CPU-oriented
tasks. Replace the placeholder binary path, export the provider key, then run:

```bash
harbor run -c benchmark/harbor/terminal-bench-2.1-dev.yaml \
  --ae DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY"
```

Before comparing agent results, run the same selected tasks with Harbor's
oracle agent. This checks that the task image and verifier work independently
of mini-harness:

```bash
harbor run \
  --repo https://github.com/harbor-framework/terminal-bench-2-1@7131e4375048a0e408a8fb404b5f499d726b695b \
  --include-task-name cancel-async-tasks \
  --include-task-name configure-git-webserver \
  --include-task-name count-dataset-tokens \
  --include-task-name fix-git \
  --include-task-name git-multibranch \
  --include-task-name log-summary-date-ranges \
  --include-task-name nginx-request-logging \
  --include-task-name regex-log \
  --include-task-name sanitize-git-repo \
  --include-task-name sqlite-db-truncate \
  --include-task-name write-compressor \
  --agent oracle
```

Treat this as a development subset, not the official 89-task score.

Harbor 0.23 filters this repository by the bare task directory name (for
example `cancel-async-tasks`), not by `terminal-bench/cancel-async-tasks`.
Run commands from the repository root with `PYTHONPATH="$PWD"` if Harbor
cannot import `benchmark.harbor.mini_harness_agent`.

### Container network access

Configuring a proxy for the Docker daemon only fixes image downloads. If the
task or verifier needs the host's proxy to download dependencies, add literal
proxy values to your local job YAML (the proxy address must be reachable from
the container):

```yaml
environment:
  env:
    HTTP_PROXY: "http://172.24.224.1:7897"
    HTTPS_PROXY: "http://172.24.224.1:7897"
    http_proxy: "http://172.24.224.1:7897"
    https_proxy: "http://172.24.224.1:7897"
    NO_PROXY: "deb.debian.org,security.debian.org,localhost,127.0.0.1,::1"
    no_proxy: "deb.debian.org,security.debian.org,localhost,127.0.0.1,::1"
```

Harbor 0.23 keeps `${VAR}` templates literal for prebuilt task images, so
replace `172.24.224.1:7897` with the proxy address reachable from your Docker
containers instead of writing `${HTTP_PROXY}` in this section. This reaches
both the agent and the verifier. Keep API keys in `--ae` or the agent's `env`
configuration, not the shared environment. For these eleven
tasks, check `verifier/test-stdout.txt` and `verifier/ctrf.json` before interpreting
the reward: some test scripts write reward zero even when dependency setup
fails before pytest starts. A zero from failed setup is not a valid task score.

## Versioning a development subset

For a reproducible Terminal-Bench development subset, record the benchmark
release, task IDs, model name, network policy, binary commit, and timeout in the
experiment notes. Do not report a hand-picked subset as the official
Terminal-Bench score. Run the benchmark's oracle/reference agent first to
separate task-environment failures from agent failures.
