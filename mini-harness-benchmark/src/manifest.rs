//! Small, deterministic benchmark manifest and result types.
//!
//! The benchmark command intentionally only owns orchestration of mini-harness
//! tasks.  It does not run a verifier and it does not know about a particular
//! benchmark suite (Terminal-Bench, SWE-bench, or otherwise).  An outer runner
//! such as Harbor can prepare a workspace, invoke the harness, and evaluate
//! the resulting files with the suite's verifier.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Version of the manifest format understood by this binary.
pub const MANIFEST_VERSION: u32 = 1;

/// A fixed set of independent tasks used for a benchmark run.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkManifest {
    /// Human-readable suite name, for example `terminal-bench-2.1-dev`.
    #[serde(default)]
    pub name: Option<String>,
    /// Manifest schema version.  Missing means version 1 for convenience.
    #[serde(default = "default_manifest_version")]
    pub version: u32,
    pub tasks: Vec<BenchmarkTask>,
}

fn default_manifest_version() -> u32 {
    MANIFEST_VERSION
}

/// One non-interactive harness invocation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkTask {
    /// Stable task id supplied by the benchmark suite.
    pub id: String,
    /// Prompt sent to the model.
    pub prompt: String,
    /// Task workspace.  Relative paths are resolved relative to the manifest.
    pub workspace: std::path::PathBuf,
    /// Optional explicit event-log path.  Otherwise the runner creates one
    /// below its artifact directory.
    #[serde(default)]
    pub event_log: Option<std::path::PathBuf>,
    /// Optional opaque metadata retained in the result for later analysis.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Why a task did not produce a successful harness turn.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Provider,
    Tool,
    Execution,
    Policy,
    ApprovalPending,
    Timeout,
    Budget,
    Cancelled,
    Durable,
    Configuration,
    Environment,
    Invariant,
    Unknown,
}

/// Machine-readable outcome for one task attempt.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkStatus {
    Completed,
    Failed,
}

/// Whether the harness itself completed.  This is deliberately independent
/// from [`TaskResult`]: a healthy harness run may still fail the benchmark's
/// verifier, while a crashed harness has no trustworthy task result.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Succeeded,
    #[default]
    Failed,
    TimedOut,
    Cancelled,
    ApprovalPending,
    EnvironmentError,
}

/// Result reported by an independent task verifier.  The built-in benchmark
/// command leaves this as `not_verified`; Harbor or another outer runner can
/// update it after checking the workspace.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskResult {
    Passed,
    Failed,
    #[default]
    NotVerified,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct BenchmarkResult {
    pub task_id: String,
    /// Legacy coarse status retained for consumers of manifest version 1.
    pub status: BenchmarkStatus,
    #[serde(default)]
    pub execution_status: ExecutionStatus,
    #[serde(default)]
    pub task_result: TaskResult,
    pub failure_category: Option<FailureCategory>,
    pub error: Option<String>,
    pub text: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub workspace: String,
    pub event_log: String,
    pub elapsed_ms: u128,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct BenchmarkResultWire {
    task_id: String,
    status: BenchmarkStatus,
    #[serde(default)]
    execution_status: Option<ExecutionStatus>,
    #[serde(default)]
    task_result: Option<TaskResult>,
    failure_category: Option<FailureCategory>,
    error: Option<String>,
    text: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    workspace: String,
    event_log: String,
    elapsed_ms: u128,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for BenchmarkResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = BenchmarkResultWire::deserialize(deserializer)?;
        let execution_status = wire.execution_status.unwrap_or(match &wire.status {
            BenchmarkStatus::Completed => ExecutionStatus::Succeeded,
            BenchmarkStatus::Failed => match &wire.failure_category {
                Some(FailureCategory::Timeout) => ExecutionStatus::TimedOut,
                Some(FailureCategory::Cancelled) => ExecutionStatus::Cancelled,
                Some(FailureCategory::ApprovalPending) => ExecutionStatus::ApprovalPending,
                Some(FailureCategory::Environment) => ExecutionStatus::EnvironmentError,
                _ => ExecutionStatus::Failed,
            },
        });
        Ok(Self {
            task_id: wire.task_id,
            status: wire.status,
            execution_status,
            task_result: wire.task_result.unwrap_or_default(),
            failure_category: wire.failure_category,
            error: wire.error,
            text: wire.text,
            session_id: wire.session_id,
            turn_id: wire.turn_id,
            workspace: wire.workspace,
            event_log: wire.event_log,
            elapsed_ms: wire.elapsed_ms,
            metadata: wire.metadata,
        })
    }
}

/// Aggregate output written by `mini-harness benchmark`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BenchmarkRun {
    pub manifest: String,
    pub manifest_name: Option<String>,
    pub manifest_version: u32,
    pub artifact_dir: String,
    pub binary_version: String,
    pub provider: String,
    pub model: Option<String>,
    /// Effective settings contain permissions and budgets, never credentials.
    pub effective_config: mini_harness::config::HarnessConfig,
    /// False in the incrementally written file until this invocation ends.
    pub finished: bool,
    pub results: Vec<BenchmarkResult>,
    pub total_tasks: usize,
    pub skipped: usize,
    pub completed: usize,
    pub failed: usize,
}

impl BenchmarkManifest {
    /// Load either TOML or JSON.  The extension is preferred; when it is not
    /// recognized TOML is attempted first and JSON second.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read manifest {}: {error}", path.display()))?;
        let extension = path.extension().and_then(|value| value.to_str());
        let manifest: Self = match extension {
            Some("json") => serde_json::from_str(&text).map_err(|error| error.to_string()),
            Some("toml") => toml::from_str(&text).map_err(|error| error.to_string()),
            _ => toml::from_str(&text)
                .map_err(|error| error.to_string())
                .or_else(|_| serde_json::from_str(&text).map_err(|error| error.to_string())),
        }
        .map_err(|error| format!("invalid benchmark manifest {}: {error}", path.display()))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(format!(
                "unsupported benchmark manifest version {}; expected {}",
                manifest.version, MANIFEST_VERSION
            ));
        }
        if manifest.tasks.is_empty() {
            return Err("benchmark manifest must contain at least one task".into());
        }
        let mut ids = std::collections::HashSet::new();
        for task in &manifest.tasks {
            if task.id.trim().is_empty() {
                return Err("benchmark task id must not be empty".into());
            }
            if task.prompt.trim().is_empty() {
                return Err(format!("benchmark task `{}` has an empty prompt", task.id));
            }
            if !ids.insert(&task.id) {
                return Err(format!("duplicate benchmark task id `{}`", task.id));
            }
        }
        Ok(manifest)
    }
}

impl BenchmarkResult {
    pub fn completed(
        task: &BenchmarkTask,
        workspace: impl Into<String>,
        event_log: impl Into<String>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        text: impl Into<String>,
        elapsed_ms: u128,
    ) -> Self {
        Self {
            task_id: task.id.clone(),
            status: BenchmarkStatus::Completed,
            execution_status: ExecutionStatus::Succeeded,
            task_result: TaskResult::NotVerified,
            failure_category: None,
            error: None,
            text: Some(text.into()),
            session_id: Some(session_id.into()),
            turn_id: Some(turn_id.into()),
            workspace: workspace.into(),
            event_log: event_log.into(),
            elapsed_ms,
            metadata: task.metadata.clone(),
        }
    }

    pub fn failed(
        task: &BenchmarkTask,
        workspace: impl Into<String>,
        event_log: impl Into<String>,
        category: FailureCategory,
        error: impl Into<String>,
        elapsed_ms: u128,
    ) -> Self {
        let execution_status = match category {
            FailureCategory::Timeout => ExecutionStatus::TimedOut,
            FailureCategory::Cancelled => ExecutionStatus::Cancelled,
            FailureCategory::Environment => ExecutionStatus::EnvironmentError,
            FailureCategory::ApprovalPending => ExecutionStatus::ApprovalPending,
            _ => ExecutionStatus::Failed,
        };
        Self {
            task_id: task.id.clone(),
            status: BenchmarkStatus::Failed,
            execution_status,
            task_result: TaskResult::NotVerified,
            failure_category: Some(category),
            error: Some(error.into()),
            text: None,
            session_id: None,
            turn_id: None,
            workspace: workspace.into(),
            event_log: event_log.into(),
            elapsed_ms,
            metadata: task.metadata.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_manifest_and_rejects_duplicate_ids() {
        let manifest: BenchmarkManifest = toml::from_str(
            r#"
name = "dev"
[[tasks]]
id = "a"
prompt = "fix it"
workspace = "/tmp/a"
"#,
        )
        .expect("manifest");
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.tasks[0].id, "a");

        let duplicate = BenchmarkManifest {
            name: None,
            version: MANIFEST_VERSION,
            tasks: vec![manifest.tasks[0].clone(), manifest.tasks[0].clone()],
        };
        let path = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(path.path(), toml::to_string(&duplicate).expect("toml")).expect("write");
        let error = BenchmarkManifest::load(path.path()).expect_err("duplicate ids");
        assert!(error.contains("duplicate benchmark task id"));
    }

    #[test]
    fn result_serializes_stable_status_names() {
        let task = BenchmarkTask {
            id: "task-1".into(),
            prompt: "run".into(),
            workspace: "/tmp/task".into(),
            event_log: None,
            metadata: BTreeMap::new(),
        };
        let result = BenchmarkResult::failed(
            &task,
            "/tmp/task",
            "/tmp/events.jsonl",
            FailureCategory::Environment,
            "workspace missing",
            4,
        );
        let json = serde_json::to_value(result).expect("json");
        assert_eq!(json["status"], "failed");
        assert_eq!(json["execution_status"], "environment_error");
        assert_eq!(json["task_result"], "not_verified");
        assert_eq!(json["failure_category"], "environment");
    }

    #[test]
    fn execution_and_task_result_are_independent() {
        let task = BenchmarkTask {
            id: "task-1".into(),
            prompt: "run".into(),
            workspace: "/tmp/task".into(),
            event_log: None,
            metadata: BTreeMap::new(),
        };
        let mut result = BenchmarkResult::completed(
            &task,
            "/tmp/task",
            "/tmp/events.jsonl",
            "session",
            "turn",
            "done",
            4,
        );
        assert_eq!(result.execution_status, ExecutionStatus::Succeeded);
        assert_eq!(result.task_result, TaskResult::NotVerified);
        result.task_result = TaskResult::Failed;
        assert_eq!(result.execution_status, ExecutionStatus::Succeeded);
    }

    #[test]
    fn legacy_result_without_split_status_remains_readable() {
        let json = r#"{
          "task_id":"task-1",
          "status":"completed",
          "failure_category":null,
          "error":null,
          "text":"done",
          "session_id":"session",
          "turn_id":"turn",
          "workspace":"/tmp/task",
          "event_log":"/tmp/events.jsonl",
          "elapsed_ms":1,
          "metadata":{}
        }"#;
        let result: BenchmarkResult = serde_json::from_str(json).expect("legacy result");
        assert_eq!(result.execution_status, ExecutionStatus::Succeeded);
        assert_eq!(result.task_result, TaskResult::NotVerified);
    }
}
