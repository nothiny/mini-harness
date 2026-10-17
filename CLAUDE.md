# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

mini-harness is an **educational** agent harness runtime built from scratch in Rust (~8k lines). It's the infrastructure around the model loop: tool execution with authorization, process management, event logging, and crash recovery.

**Core principle**: Event sourcing as the single source of truth. A JSONL event log is the only fact; a deterministic reducer rebuilds all state from events. Replay twice → identical state.

The repository is a Cargo workspace:

- `.` (mini-harness) — the teaching core: durable event log, agent loop, tools, executor
- `mini-harness-server/` — optional JSONL protocol server (concurrency on top of the synchronous `Session`)
- `mini-harness-benchmark/` — optional benchmark runner
- `ui/` — Ink TUI (TypeScript), talks to `mini-harness-server --stdio`

## Key Architectural Concepts

### Event-Sourced State

- **JSONL event log** (`durable/event_log.rs`): Append-only log of all operations, with incremental suffix validation across processes
- **Deterministic reducer** (`durable/reducer.rs`): Rebuilds `SessionState` from events; illegal transitions fail loudly instead of "repairing" the log
- **No checkpoints**: Recovery is always a full replay from the event log (deliberate simplification)

### Synchronous Session (no Actor)

- **Session** (`runtime/session.rs`): A single-task, sequentially driven facade over `AgentLoop`. One turn at a time; a second `start_turn` while a turn is active returns `TurnAlreadyActive`
- **Order guarantee**: Validate → persist event → commit state transition (in `AgentLoop::append`)
- Concurrency (mid-turn cancel/approve) is composed *outside* the core — see `mini-harness-server`

### Process Execution

- **Executor trait** (`executor/trait.rs`): Abstract interface for command execution
- **LocalExecutor** (`executor/local.rs`): Process group-level cancellation, concurrent stdout/stderr draining, UTF-8-safe truncation
- **Global concurrency limit**: Shared semaphore across all tool executions
- **Three-layer output limits**: per-tool, per-turn cumulative, and line-level truncation

### Tool Chain

- **Tool registry** (`tools/registry.rs`): Dynamic registration with spec validation
- **Three core tools**: `read_workspace`, `edit_workspace`, `bash`
- **Merged tool events**: each call produces exactly `tool.requested` (carries `requires_approval`) + `tool.completed` (outcome: succeeded/rejected/failed). User decisions are separate `tool.approval.responded` events

### Provider Abstraction

- **ModelProvider trait** (`model/provider.rs`): Common interface for AI providers
- **MockProvider** (`model/mock.rs`): Deterministic responses for offline testing
- **OpenAI protocol support** (`model/openai.rs`, `model/openai_protocol.rs`): Responses API + Chat Completions (DeepSeek)

Both providers pass the same behavioral contract tests (`tests/provider_contract.rs`).

### Permission & Policy

- **ToolPolicy trait** (`policy/mod.rs`): `allow` or `ask` per tool (no `deny` — refusal is always "ask + user denies", so the log records exactly one kind of refusal fact)
- **Approval flow**: `tool.requested {requires_approval: true}` → turn pauses with `ApprovalPending` → user responds → `tool.approval.responded` → execute (approved) or `turn.cancelled` (denied)
- Approvals survive crashes (pending approvals are just events in the log)

## Development Commands

### Build & Test

```bash
make check                 # fmt + clippy + tests for the whole workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace     # all tests are offline
```

### Running Sessions

```bash
cargo run -- demo read README.md          # offline demo (MockProvider, in-memory log)
cargo run -- run "your prompt" --provider deepseek --workspace .
cargo run -- resume <session-id> "follow-up prompt"
cargo run -- cancel <session-id> <turn-id>

# Inspection
cargo run -- events <session-id>          # view the event log
cargo run -- inspect <session-id>         # replay and print state
cargo run -- sessions                     # list sessions
cargo run -- config                       # effective configuration
cargo run -- doctor                       # environment check

# Teaching examples
cargo run --example 01_event_sourcing
cargo run --example 02_agent_loop
cargo run --example 03_tool_execution
cargo run --example 04_crash_recovery
```

### Companion crates

```bash
# JSONL protocol server (used by the Ink TUI)
cargo run -p mini-harness-server -- --stdio

# Benchmark runner
cargo run -p mini-harness-benchmark -- mini-harness-benchmark/tasks/tasks.example.toml --provider mock
```

### TUI

```bash
make tui              # Ink TUI with DeepSeek (spawns mini-harness-server)
make tui-demo         # offline demo mode (no server, fixed responses)
cd ui && npm run typecheck && npm test
```

## Code Structure

```
src/
├── main.rs               # CLI entry point (run/resume/cancel/events/inspect/sessions/demo/config/doctor)
├── lib.rs                # Public module tree
├── config.rs             # TOML config parser with layering
├── error.rs              # Error types (HarnessError, DurableError, ToolError)
├── durable/              # Event sourcing core
│   ├── event.rs          # EventPayload enum (schema 3, merged tool events)
│   ├── event_log.rs      # JSONL append-only log with atomic writes
│   ├── reducer.rs        # Deterministic state reconstruction
│   ├── store.rs          # EventStore trait
│   └── memory.rs         # In-memory store for tests/demos
├── runtime/              # Session lifecycle & agent loop
│   ├── session.rs        # Synchronous Session facade
│   ├── agent_loop.rs     # Main model-tool execution loop
│   ├── agent_loop_approval.rs  # Approval resume logic
│   ├── agent_loop_setup.rs     # Constructors + restore-by-replay
│   ├── sampling.rs       # Provider sampling with attempt telemetry
│   ├── state.rs          # SessionState & turn tracking
│   ├── context.rs        # Context window management
│   └── ids.rs            # Type-safe IDs (SessionId, TurnId, ToolCallId)
├── executor/             # Process execution layer
│   ├── trait.rs          # Executor trait
│   ├── local.rs          # Real process execution with limits
│   ├── process.rs        # Process groups & cancellation
│   ├── filesystem.rs     # Workspace boundary enforcement
│   └── filesystem_secure.rs  # Secure path operations (no symlink escapes)
├── model/                # Provider abstraction
│   ├── provider.rs       # ModelProvider trait
│   ├── mock.rs           # Deterministic test provider
│   ├── openai.rs         # OpenAI/DeepSeek implementation
│   └── openai_protocol.rs  # Wire protocol types
├── tools/                # Tool registry & implementations
│   ├── registry.rs, spec.rs, read.rs, edit.rs, bash.rs
└── policy/               # allow/ask permission system

mini-harness-server/      # JSONL protocol server (jsonl.rs, projection.rs, transport.rs)
mini-harness-benchmark/   # Benchmark runner (manifest.rs, main.rs, tasks/)
ui/                       # Ink TUI (TypeScript)
examples/                 # 01..04 teaching examples
```

## Testing Philosophy

All core tests are **offline** (no network required):

- **Provider contract tests** (`tests/provider_contract.rs`): both Mock and real providers pass the same scenarios
- **Property tests** (`tests/property_replay.rs`): seeded random scripts verify replay determinism
- **Cross-process tests** (`tests/jsonl_cross_process.rs`, `tests/cross_process_cancel.rs`, `tests/session_cancel.rs`): multi-process append/cancel probes
- **Integration tests** (`tests/integration_agent_loop.rs`): full agent loop with mock provider
- **Tool tests** (`tests/{read,edit,bash}_tool.rs`): tool boundary enforcement
- **Protocol tests** (`mini-harness-server/tests/`): JSONL wire format against the server binary

Run all with: `cargo test --workspace`

## Important Design Patterns

### Cross-Process Cancellation

- `cancel` command writes terminal events to the log
- Running process detects log change on next append attempt
- Fails with durable error rather than overwriting concurrent writes
- **Principle**: Fail rather than clobber another writer's work

### Output Limits

Three layers prevent unbounded memory growth:

1. **Per-tool limit**: `max_output_bytes` in executor config
2. **Per-turn cumulative**: `max_tool_result_bytes` in agent loop config
3. **Line-level truncation**: UTF-8-aware, never splits multibyte chars

### Observability

- **Diagnostic logs → stderr only** (stdout reserved for CLI JSON output / protocol)
- **Default log level**: `warn`
- **Debug mode**: `RUST_LOG=mini_harness=debug`

## Configuration Layers

Priority: **CLI flags** > **file** > **defaults**

Five sections in TOML:

```toml
[model]          # Provider & model selection
[session]        # Turn limits, workspace path
[execution]      # Process timeouts, output limits, concurrency
[durable]        # Event log root path, fsync mode
[permissions]    # Tool policy: allow/ask per tool (no deny)
```

Unknown fields are rejected with line numbers to catch typos early.

## Important Constraints

- **Rust edition 2024**: Requires Rust 1.85+
- **Event log is append-only**: Never edit by hand
- **Event schema version is 3**: legacy (1/2) logs are rejected — the simplified event shapes differ
- **Workspace boundary is enforced**: Tools cannot escape workspace path
- **Process groups**: All child processes are killed on cancellation
- **No interactive git**: Tools run non-interactively

## Related Documentation

- `README.md`: User-facing quickstart and learning path (Chinese)
- `SIMPLIFICATION_GUIDE.md` / `SIMPLIFICATION_PROPOSAL.md`: the simplification this codebase went through
- `mini-harness-benchmark/tasks/harbor/README.md`: Harbor adapter for Terminal-Bench 2.1
