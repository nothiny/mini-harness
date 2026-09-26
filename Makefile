# mini-harness — development commands
# CI (.github/workflows/ci.yml) runs the same three checks on macOS/Linux.

BINARY   := target/debug/mini-harness
UI_DIR   := ui
DEEPSEEK_MODEL := deepseek-flash

.PHONY: fmt fmt-check clippy test check check-docs build run tui tui-rs events serve doctor

# ── Rust baseline ──────────────────────────────────────────────────────────

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test

check: fmt-check clippy test

## check-docs: verify documentation references (test names, source paths, links)
check-docs:
	bash scripts/check-docs.sh

build:
	cargo build

$(BINARY):
	cargo build

# ── Run with DeepSeek (default) ────────────────────────────────────────────

## run: build + one-shot turn via DeepSeek (usage: make run P="your prompt")
run: $(BINARY)
	$(BINARY) run "$(P)" --provider deepseek --model $(DEEPSEEK_MODEL)

## run-mock: one-shot turn via mock provider (offline, no API key needed)
run-mock: $(BINARY)
	$(BINARY) run "$(P)"

# ── TUI ────────────────────────────────────────────────────────────────────

## tui: terminal ChatGPT with DeepSeek (Ink, fresh session per launch)
tui: $(BINARY)
	cd $(UI_DIR) && npx tsx src/main.tsx \
		--provider deepseek --model $(DEEPSEEK_MODEL) \
		--binary ../$(BINARY)

## tui-rs: the Rust-built TUI (no Node dependency, auto line-mode in pipes)
tui-rs: $(BINARY)
	$(BINARY) tui --provider deepseek --model $(DEEPSEEK_MODEL)

# ── Utility ─────────────────────────────────────────────────────────────────

## events: one-shot event viewer via mock (usage: make events P="hello")
events: $(BINARY)
	$(BINARY) events "$(P)"

## serve: start the JSONL protocol server on stdio
serve: $(BINARY)
	$(BINARY) serve --stdio

## doctor: check environment (API keys, durable root, tool registration)
doctor: $(BINARY)
	$(BINARY) doctor

# ── Help ────────────────────────────────────────────────────────────────────

help: ## show available targets
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## //'
