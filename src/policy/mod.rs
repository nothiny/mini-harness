//! Execution policy decisions made before a tool is started.
//!
//! 简化版只有两种决定：
//!
//! * [`PolicyDecision::Allow`] — 直接执行
//! * [`PolicyDecision::Ask`] — 先持久化审批请求，等待用户决定
//!
//! 没有单独的 "Deny"：拒绝统一表示为「Ask + 用户拒绝」，事件日志里
//! 只会出现 `tool.approval.responded { approved: false }` 一种拒绝事实。
//! 工作区边界不需要策略层把关 —— `read_workspace`/`edit_workspace`
//! 在执行器层强制执行路径约束。

use crate::tools::{RiskClass, ToolSpec};
use serde_json::Value;

/// The decision that the runtime must apply before starting a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyDecision {
    /// Start the tool immediately.
    Allow,
    /// Persist an approval request and leave the execution pending.
    Ask,
}

/// A configured mode for one tool class, loaded from `[permissions]`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Allow,
    Ask,
}

/// Decides whether a model-proposed tool call may start.
///
/// Implementations should be deterministic. The runtime owns durable
/// approval events; a policy only returns the decision for one call.
pub trait ToolPolicy: Send + Sync {
    fn decide(&self, spec: &ToolSpec, input: &Value) -> PolicyDecision;
}

/// Safe default policy for a local workspace.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultPolicy;

impl ToolPolicy for DefaultPolicy {
    fn decide(&self, spec: &ToolSpec, input: &Value) -> PolicyDecision {
        match spec.risk {
            RiskClass::Read => PolicyDecision::Allow,
            RiskClass::Write => PolicyDecision::Ask,
            RiskClass::Execute => {
                // Read-only commands are auto-approved; anything that could
                // modify state, access the network, or execute arbitrary
                // code still requires explicit approval.
                if is_readonly_command(input) {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::Ask
                }
            }
        }
    }
}

/// Explicit opt-in policy useful for controlled local demos and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllPolicy;

impl ToolPolicy for AllowAllPolicy {
    fn decide(&self, _spec: &ToolSpec, _input: &Value) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

/// Commands that only read state and cannot modify anything.
/// Auto-approved by DefaultPolicy; everything else still asks.
fn is_readonly_command(input: &Value) -> bool {
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return false;
    };

    // Shell metacharacters that could cause side effects mean the command
    // is NOT read-only regardless of the first word.
    if command.contains('>')
        || command.contains("&&")
        || command.contains(';')
        || command.contains("$(")
        || command.contains('`')
        || command.contains(">>")
    {
        return false;
    }

    // Check every subcommand in a pipeline (a | b | c).
    for segment in command.split('|') {
        let mut words = segment.split_whitespace();
        let Some(first) = words.next() else {
            continue; // empty segment (e.g., leading |)
        };
        let sub = words.next().unwrap_or("");
        if !is_readonly_word(first, sub) {
            return false;
        }
    }
    true
}

fn is_readonly_word(first: &str, sub: &str) -> bool {
    const READONLY: &[&str] = &[
        "ls",
        "cat",
        "head",
        "tail",
        "grep",
        "find",
        "pwd",
        "wc",
        "which",
        "file",
        "stat",
        "du",
        "df",
        "echo",
        "printf",
        "whoami",
        "hostname",
        "uname",
        "date",
        "env",
        "printenv",
        "type",
        "command",
        "hash",
        "tree",
        "diff",
        "cmp",
        "sort",
        "uniq",
        "cut",
        "column",
        "bat",
        "rg",
        "fd",
        "jq",
        "yq",
        "tokei",
        "hyperfine",
    ];
    if READONLY.contains(&first) {
        return true;
    }

    match first {
        "git" => matches!(
            sub,
            "status"
                | "log"
                | "diff"
                | "show"
                | "branch"
                | "tag"
                | "remote"
                | "blame"
                | "shortlog"
                | "describe"
                | "stash"
                | "rev-parse"
                | "ls-files"
                | "ls-remote"
                | "config"
                | "--get"
                | "reflog"
                | "count-objects"
        ),
        "cargo" => matches!(
            sub,
            "check" | "test" | "build" | "metadata" | "tree" | "fmt" | "clippy" | "doc"
        ),
        "rustup" => matches!(sub, "show" | "list" | "doc"),
        "npm" | "npx" | "yarn" | "pnpm" => {
            matches!(
                sub,
                "list" | "ls" | "outdated" | "info" | "why" | "run" | "test"
            )
        }
        "go" => matches!(sub, "version" | "list" | "doc" | "env" | "vet" | "test"),
        "docker" => matches!(
            sub,
            "ps" | "images" | "version" | "info" | "logs" | "inspect"
        ),
        _ => false,
    }
}
