# mini-harness — development commands
# CI (.github/workflows/ci.yml) runs the same checks on macOS/Linux.

BINARY       := target/debug/mini-harness
SERVER_BIN   := target/debug/mini-harness-server
BENCH_BIN    := target/debug/mini-harness-benchmark
UI_DIR       := ui
DEEPSEEK_MODEL := deepseek-flash

.PHONY: fmt fmt-check clippy test check build run tui tui-demo serve doctor bench

# ── Rust baseline ──────────────────────────────────────────────────────────

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace

check: fmt-check clippy test

build:
	cargo build --workspace

$(BINARY):
	cargo build -p mini-harness

$(SERVER_BIN):
	cargo build -p mini-harness-server

$(BENCH_BIN):
	cargo build -p mini-harness-benchmark

# ── Run with DeepSeek (default) ────────────────────────────────────────────

## run: build + one-shot turn via DeepSeek (usage: make run P="your prompt")
run: $(BINARY)
	$(BINARY) run "$(P)" --provider deepseek --model $(DEEPSEEK_MODEL)

## run-mock: one-shot turn via mock provider (offline, no API key needed)
run-mock: $(BINARY)
	$(BINARY) run "$(P)"

# ── TUI (Ink, TypeScript) ──────────────────────────────────────────────────

## tui: terminal ChatGPT with DeepSeek (Ink, fresh session per launch)
tui: $(SERVER_BIN)
	cd $(UI_DIR) && npx tsx src/main.tsx \
		--provider deepseek --model $(DEEPSEEK_MODEL) \
		--binary ../$(SERVER_BIN)

## tui-demo: offline Ink UI demonstration using the deterministic fake client
tui-demo:
	cd $(UI_DIR) && npm run demo

# ── Utility ─────────────────────────────────────────────────────────────────

## serve: start the JSONL protocol server (mini-harness-server) on stdio
serve: $(SERVER_BIN)
	$(SERVER_BIN) --stdio

## doctor: check environment (API keys, durable root, tool registration)
doctor: $(BINARY)
	$(BINARY) doctor

## bench: run a benchmark manifest (usage: make bench M=tasks/tasks.example.toml)
bench: $(BENCH_BIN)
	$(BENCH_BIN) $(M) --provider deepseek --model $(DEEPSEEK_MODEL)

# ── Help ────────────────────────────────────────────────────────────────────

help: ## show available targets
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## //'
