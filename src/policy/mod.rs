//! Execution policy decisions made before a tool is started.

use crate::{
    runtime::WorkspaceRoot,
    tools::{RiskClass, ToolSpec},
};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// The decision that the runtime must apply before starting a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyDecision {
    /// Start the tool immediately.
    Allow,
    /// Persist an approval request and leave the execution pending.
    Ask,
    /// Persist a denied approval and do not start the tool.
    Deny,
}

/// A configured mode for one tool class, loaded from `[permissions]`
/// (design §16). Mirrors [`PolicyDecision`] minus the per-call approval
/// request that the runtime constructs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Allow,
    Ask,
    Deny,
}

/// Decides whether a model-proposed tool call may start.
///
/// Implementations should be deterministic and fail closed. The runtime owns
/// durable approval events; a policy only returns the decision for one call.
pub trait ToolPolicy: Send + Sync {
    fn decide(
        &self,
        spec: &ToolSpec,
        input: &Value,
        workspace: Option<&WorkspaceRoot>,
    ) -> PolicyDecision;
}

/// Safe default policy for a local workspace.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultPolicy;

impl ToolPolicy for DefaultPolicy {
    fn decide(
        &self,
        spec: &ToolSpec,
        input: &Value,
        workspace: Option<&WorkspaceRoot>,
    ) -> PolicyDecision {
        match spec.risk {
            RiskClass::Read => {
                let Some(workspace) = workspace else {
                    return PolicyDecision::Deny;
                };
                if path_is_within_workspace(input, workspace) {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::Deny
                }
            }
            RiskClass::Write => {
                let Some(workspace) = workspace else {
                    return PolicyDecision::Deny;
                };
                if path_is_within_workspace(input, workspace) {
                    PolicyDecision::Ask
                } else {
                    PolicyDecision::Deny
                }
            }
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

pub(crate) fn path_is_within_workspace(input: &Value, workspace: &WorkspaceRoot) -> bool {
    let Some(path) = input.get("path").and_then(Value::as_str) else {
        return false;
    };
    if path.trim().is_empty() {
        return false;
    }
    let root = if workspace.0.is_absolute() {
        lexical_normalize(&workspace.0)
    } else {
        let Ok(current_dir) = std::env::current_dir() else {
            return false;
        };
        lexical_normalize(&current_dir.join(&workspace.0))
    };
    let requested = Path::new(path);
    let candidate = if requested.is_absolute() {
        lexical_normalize(requested)
    } else {
        lexical_normalize(&root.join(requested))
    };
    candidate.starts_with(&root)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Explicit opt-in policy useful for controlled local demos and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllPolicy;

impl ToolPolicy for AllowAllPolicy {
    fn decide(
        &self,
        _spec: &ToolSpec,
        _input: &Value,
        _workspace: Option<&WorkspaceRoot>,
    ) -> PolicyDecision {
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
        "neofetch",
        "fastfetch",
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
            "check"
                | "test"
                | "build"
                | "metadata"
                | "tree"
                | "fmt"
                | "clippy"
                | "doc"
                | "verify-project"
                | "search"
                | "info"
        ),
        "rustup" => matches!(sub, "show" | "list" | "doc"),
        "npm" | "npx" | "yarn" | "pnpm" => matches!(
            sub,
            "list" | "ls" | "outdated" | "info" | "why" | "run" | "test"
        ),
        "go" => matches!(sub, "version" | "list" | "doc" | "env" | "vet" | "test"),
        "docker" => matches!(
            sub,
            "ps" | "images" | "version" | "info" | "logs" | "inspect"
        ),
        _ => false,
    }
}
