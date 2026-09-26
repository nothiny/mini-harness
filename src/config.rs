//! Effective configuration for the harness (design §16).
//!
//! The documented TOML file is the configuration boundary:
//!
//! ```toml
//! [model]
//! provider = "mock"
//! name = "test-model"
//!
//! [session]
//! workspace = "."
//! max_steps = 20
//! max_turn_time_ms = 600000
//!
//! [execution]
//! max_output_bytes = 200000
//! default_timeout_ms = 120000
//!
//! [durable]
//! root = "~/.mini-harness"
//! flush = "buffered"
//! checkpoint_every_events = 50
//!
//! [permissions]
//! read_workspace = "allow"
//! edit_workspace = "ask"
//! bash = "ask"
//! ```
//!
//! Layering: defaults ← TOML file ← CLI flags. Unknown fields are rejected
//! with their line numbers, and the effective summary never includes API
//! keys (the key is read from the environment by the provider, not here).

use crate::{
    error::ConfigError,
    policy::{PermissionMode, ToolPolicy},
    runtime::{ids::SessionId, types::WorkspaceRoot},
    tools::ToolSpec,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_FILE: &str = "mini-harness.toml";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub name: Option<String>,
}

fn default_provider() -> String {
    "mock".into()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            name: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    pub workspace: Option<PathBuf>,
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_max_turn_time_ms")]
    pub max_turn_time_ms: u64,
    #[serde(default = "default_max_batch_tool_calls")]
    pub max_batch_tool_calls: usize,
}

fn default_max_steps() -> usize {
    20
}
fn default_max_turn_time_ms() -> u64 {
    600_000
}
fn default_max_batch_tool_calls() -> usize {
    16
}

/// Derived `Default` would yield zero for every numeric field (which have
/// "disabled/reject" semantics); the documented defaults are these.
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            workspace: None,
            max_steps: default_max_steps(),
            max_turn_time_ms: default_max_turn_time_ms(),
            max_batch_tool_calls: default_max_batch_tool_calls(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_timeout_ms")]
    pub default_timeout_ms: u64,
    #[serde(default = "default_max_concurrent_processes")]
    pub max_concurrent_processes: usize,
}

fn default_max_output_bytes() -> usize {
    200_000
}
fn default_timeout_ms() -> u64 {
    120_000
}
fn default_max_concurrent_processes() -> usize {
    8
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: default_max_output_bytes(),
            default_timeout_ms: default_timeout_ms(),
            max_concurrent_processes: default_max_concurrent_processes(),
        }
    }
}

/// Durability of event-log appends (design §11.1 `durability`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushMode {
    /// Flush to the OS; do not force a device sync per append.
    #[default]
    Buffered,
    /// `fsync` after every append. Slower, survives power loss.
    Synced,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableConfig {
    pub root: Option<PathBuf>,
    #[serde(default)]
    pub flush: FlushMode,
    #[serde(default = "default_checkpoint_every_events")]
    pub checkpoint_every_events: u64,
}

fn default_checkpoint_every_events() -> u64 {
    50
}

impl Default for DurableConfig {
    fn default() -> Self {
        Self {
            root: None,
            flush: FlushMode::Buffered,
            checkpoint_every_events: default_checkpoint_every_events(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    #[serde(default = "default_read_workspace")]
    pub read_workspace: PermissionMode,
    #[serde(default = "default_edit_workspace")]
    pub edit_workspace: PermissionMode,
    #[serde(default = "default_bash")]
    pub bash: PermissionMode,
}

fn default_read_workspace() -> PermissionMode {
    PermissionMode::Allow
}
fn default_edit_workspace() -> PermissionMode {
    PermissionMode::Ask
}
fn default_bash() -> PermissionMode {
    PermissionMode::Ask
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            read_workspace: default_read_workspace(),
            edit_workspace: default_edit_workspace(),
            bash: default_bash(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub durable: DurableConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessConfigFile {
    #[serde(default)]
    model: ModelConfig,
    #[serde(default)]
    session: SessionConfig,
    #[serde(default)]
    execution: ExecutionConfig,
    #[serde(default)]
    durable: DurableConfig,
    #[serde(default)]
    permissions: PermissionsConfig,
}

impl HarnessConfig {
    /// Loads defaults, then overlays the TOML file when it exists.
    ///
    /// A missing file is not an error (defaults apply); a present-but-invalid
    /// file is, with the parser's line information preserved.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|error| {
            ConfigError::from(format!("cannot read {}: {error}", path.display()))
        })?;
        let file: HarnessConfigFile = toml::from_str(&text).map_err(|error| {
            ConfigError::from(format!("invalid config {}: {error}", path.display()))
        })?;
        Ok(Self {
            model: file.model,
            session: file.session,
            execution: file.execution,
            durable: file.durable,
            permissions: file.permissions,
        })
    }

    /// The durable root: `~/.mini-harness` by default, `~` expanded when literal.
    pub fn durable_root(&self) -> PathBuf {
        match &self.durable.root {
            Some(root) => expand_home(root),
            None => default_durable_root(),
        }
    }

    /// The documented per-session layout (design §11.1):
    /// `<root>/sessions/<session-id>/events.jsonl`.
    pub fn session_dir(&self, session_id: SessionId) -> PathBuf {
        self.durable_root()
            .join("sessions")
            .join(session_id.to_string())
    }

    pub fn session_event_log(&self, session_id: SessionId) -> PathBuf {
        self.session_dir(session_id).join("events.jsonl")
    }

    /// `<root>/sessions/<session-id>/checkpoints/latest.json`.
    pub fn session_checkpoint(&self, session_id: SessionId) -> PathBuf {
        self.session_dir(session_id)
            .join("checkpoints")
            .join("latest.json")
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self)
            .map_err(|error| ConfigError::from(format!("cannot serialize config: {error}")))
    }

    /// Maps the `[session]`/`[execution]` sections onto the agent-loop
    /// resource limits. Zero durations keep the "limit disabled" meaning
    /// documented on [`AgentLoopConfig`](crate::runtime::agent_loop::AgentLoopConfig).
    pub fn agent_loop_config(&self) -> crate::runtime::agent_loop::AgentLoopConfig {
        crate::runtime::agent_loop::AgentLoopConfig {
            max_steps: self.session.max_steps,
            max_turn_duration: std::time::Duration::from_millis(self.session.max_turn_time_ms),
            max_tool_time: std::time::Duration::from_millis(self.session.max_turn_time_ms),
            max_batch_tool_calls: self.session.max_batch_tool_calls,
            max_tool_result_bytes: self.execution.max_output_bytes,
            ..Default::default()
        }
    }
}

pub fn default_durable_root() -> PathBuf {
    home_dir().join(".mini-harness")
}

fn home_dir() -> PathBuf {
    #[cfg(unix)]
    let home = std::env::var_os("HOME");
    #[cfg(not(unix))]
    let home = std::env::var_os("USERPROFILE");
    home.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(first) = path.iter().next() else {
        return path.to_path_buf();
    };
    if first != "~" {
        return path.to_path_buf();
    }
    let mut expanded = home_dir();
    for component in path.iter().skip(1) {
        expanded.push(component);
    }
    expanded
}

/// Resolves a session reference used by `inspect`/`recover`/`abandon-turn`:
/// either a durable-layout session id or an explicit event-log path.
pub fn resolve_event_log(config: &HarnessConfig, reference: &str) -> PathBuf {
    if let Ok(session_id) = SessionId::parse(reference) {
        return config.session_event_log(session_id);
    }
    PathBuf::from(reference)
}

/// Policy driven by the `[permissions]` config section (design §10.3).
#[derive(Clone, Debug, Default)]
pub struct ConfiguredPolicy {
    pub permissions: PermissionsConfig,
}

impl ConfiguredPolicy {
    pub fn new(permissions: PermissionsConfig) -> Self {
        Self { permissions }
    }
}

impl ToolPolicy for ConfiguredPolicy {
    fn decide(
        &self,
        spec: &ToolSpec,
        input: &serde_json::Value,
        workspace: Option<&WorkspaceRoot>,
    ) -> crate::policy::PolicyDecision {
        use crate::policy::PolicyDecision;
        let mode = match spec.risk {
            crate::tools::RiskClass::Read => self.permissions.read_workspace,
            crate::tools::RiskClass::Write => self.permissions.edit_workspace,
            crate::tools::RiskClass::Execute => self.permissions.bash,
        };
        // Path-scoped tools stay bound to the workspace regardless of the
        // configured mode: outside paths are always denied (fail closed).
        let path_scoped = matches!(
            spec.risk,
            crate::tools::RiskClass::Read | crate::tools::RiskClass::Write
        );
        if path_scoped {
            let Some(workspace) = workspace else {
                return PolicyDecision::Deny;
            };
            if !crate::policy::path_is_within_workspace(input, workspace) {
                return PolicyDecision::Deny;
            }
        }
        match mode {
            PermissionMode::Allow => PolicyDecision::Allow,
            PermissionMode::Ask => PolicyDecision::Ask,
            PermissionMode::Deny => PolicyDecision::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_round_trips_and_rejects_unknown_fields() {
        let text = r#"
[model]
provider = "openai"
name = "gpt-5"

[session]
max_steps = 7

[execution]
default_timeout_ms = 45000

[permissions]
bash = "allow"
"#;
        let file = std::env::temp_dir().join("mini-harness-config-test.toml");
        std::fs::write(&file, text).unwrap();
        let config = HarnessConfig::load(Some(&file)).unwrap();
        assert_eq!(config.model.provider, "openai");
        assert_eq!(config.model.name.as_deref(), Some("gpt-5"));
        assert_eq!(config.session.max_steps, 7);
        assert_eq!(config.session.max_turn_time_ms, 600_000);
        assert_eq!(config.execution.default_timeout_ms, 45_000);
        assert_eq!(config.permissions.bash, PermissionMode::Allow);
        assert_eq!(config.permissions.edit_workspace, PermissionMode::Ask);
        assert_eq!(config.durable.flush, FlushMode::Buffered);
        std::fs::remove_file(&file).ok();

        let bad = std::env::temp_dir().join("mini-harness-config-bad.toml");
        std::fs::write(&bad, "[session]\nmax_stepz = 3\n").unwrap();
        let error = HarnessConfig::load(Some(&bad)).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
        std::fs::remove_file(&bad).ok();
    }

    #[test]
    fn home_is_expanded_in_durable_root() {
        let config = HarnessConfig {
            durable: DurableConfig {
                root: Some("~/somewhere".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let root = config.durable_root();
        assert!(!root.starts_with("~"));
        assert!(root.ends_with("somewhere"));
    }
}
