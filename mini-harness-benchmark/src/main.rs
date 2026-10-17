//! `mini-harness-bench`: 以固定、非交互的方式运行任务清单并输出
//! 机器可读结果。所有工具权限强制为 allow，一次调用绝不会等待人工。

use clap::{Args, Parser, ValueEnum};
use mini_harness::{
    durable::{EventPayload, EventStore, JsonlEventStore},
    executor::LocalExecutor,
    model::ConfiguredProvider,
    runtime::{EventSeq, SessionId, session::Session},
    tools::{BashTool, EditTool, ReadTool, ToolRegistry},
};
use mini_harness_benchmark::{
    BenchmarkManifest, BenchmarkResult, BenchmarkRun, BenchmarkStatus, FailureCategory,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[derive(Parser)]
#[command(
    name = "mini-harness-bench",
    version,
    about = "Run a mini-harness benchmark manifest"
)]
struct Cli {
    #[command(flatten)]
    args: BenchArgs,
}

#[derive(Args)]
struct BenchArgs {
    /// TOML or JSON manifest containing independent benchmark tasks.
    #[arg(value_name = "MANIFEST")]
    manifest: Option<PathBuf>,
    /// Long-form spelling useful from scripts.
    #[arg(long = "manifest", conflicts_with = "manifest")]
    manifest_flag: Option<PathBuf>,
    /// Directory for event logs when a task does not provide an explicit
    /// `event_log` path.
    #[arg(long, default_value = ".mini-harness-benchmark")]
    artifacts: PathBuf,
    /// Optional path for the aggregate JSON result. stdout always receives
    /// the same document.
    #[arg(long)]
    results: Option<PathBuf>,
    /// Do not start tasks after the first failed harness invocation.
    #[arg(long)]
    fail_fast: bool,
    /// Path to the TOML configuration file (defaults to ./mini-harness.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(flatten)]
    provider: ProviderArgs,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProviderChoice {
    Mock,
    Openai,
    Deepseek,
}

#[derive(Clone, Args)]
struct ProviderArgs {
    #[arg(long, value_enum)]
    provider: Option<ProviderChoice>,
    #[arg(long)]
    model: Option<String>,
}

impl ProviderArgs {
    fn resolve(&self, config: &mini_harness::config::HarnessConfig) -> (String, Option<String>) {
        let provider = match self.provider {
            Some(ProviderChoice::Mock) => "mock".to_owned(),
            Some(ProviderChoice::Openai) => "openai".to_owned(),
            Some(ProviderChoice::Deepseek) => "deepseek".to_owned(),
            None => config.model.provider.clone(),
        };
        let model = self.model.clone().or_else(|| config.model.name.clone());
        (provider, model)
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let args = cli.args;
    let manifest_input = args
        .manifest
        .or(args.manifest_flag)
        .ok_or_else(|| "usage: mini-harness-bench <manifest> (or --manifest <path>)".to_owned())?;
    let manifest_path = std::fs::canonicalize(&manifest_input).map_err(|error| {
        format!(
            "cannot resolve manifest {}: {error}",
            manifest_input.display()
        )
    })?;
    let manifest = BenchmarkManifest::load(&manifest_path)?;
    let config_path = args.config.clone().or_else(|| {
        Path::new(mini_harness::config::DEFAULT_CONFIG_FILE)
            .exists()
            .then(|| PathBuf::from(mini_harness::config::DEFAULT_CONFIG_FILE))
    });
    let config = mini_harness::config::HarnessConfig::load(config_path.as_deref())
        .map_err(|error| error.to_string())?;
    let (provider_name, selected_model) = args.provider.resolve(&config);
    let model = match (provider_name.as_str(), selected_model) {
        ("deepseek", None) => Some("deepseek-chat".to_owned()),
        (_, model) => model,
    };
    let artifacts = if args.artifacts.is_absolute() {
        args.artifacts.clone()
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(&args.artifacts)
    };
    // Keep attempts separate so rerunning the same manifest never appends to
    // an earlier event log.
    let artifacts = artifacts.join(format!("run-{}", SessionId::new()));
    tokio::fs::create_dir_all(&artifacts)
        .await
        .map_err(|error| format!("cannot create benchmark artifacts: {error}"))?;

    let mut benchmark_config = config.clone();
    benchmark_config.permissions.read_workspace = mini_harness::policy::PermissionMode::Allow;
    benchmark_config.permissions.edit_workspace = mini_harness::policy::PermissionMode::Allow;
    benchmark_config.permissions.bash = mini_harness::policy::PermissionMode::Allow;
    benchmark_config.model.provider = provider_name.clone();
    benchmark_config.model.name = model.clone();
    if benchmark_config.session.max_steps == 0 || benchmark_config.session.max_turn_time_ms == 0 {
        return Err(
            "benchmark requires positive session.max_steps and session.max_turn_time_ms budgets"
                .into(),
        );
    }

    tokio::fs::write(
        artifacts.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .await
    .map_err(|error| format!("cannot preserve benchmark manifest: {error}"))?;
    tokio::fs::write(
        artifacts.join("effective-config.toml"),
        benchmark_config
            .to_toml()
            .map_err(|error| error.to_string())?,
    )
    .await
    .map_err(|error| format!("cannot preserve benchmark configuration: {error}"))?;

    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut run = BenchmarkRun {
        manifest: manifest_path.display().to_string(),
        manifest_name: manifest.name.clone(),
        manifest_version: manifest.version,
        artifact_dir: artifacts.display().to_string(),
        binary_version: env!("CARGO_PKG_VERSION").into(),
        provider: provider_name,
        model,
        effective_config: benchmark_config.clone(),
        finished: false,
        results: Vec::with_capacity(manifest.tasks.len()),
        total_tasks: manifest.tasks.len(),
        skipped: manifest.tasks.len(),
        completed: 0,
        failed: 0,
    };
    write_benchmark_results(&run, &artifacts, args.results.as_deref()).await?;
    for (task_index, task) in manifest.tasks.iter().enumerate() {
        let started = Instant::now();
        let workspace = resolve_benchmark_path(&manifest_dir, &task.workspace);
        let event_log = task
            .event_log
            .as_ref()
            .map(|path| resolve_benchmark_path(&manifest_dir, path))
            .unwrap_or_else(|| {
                artifacts.join(format!(
                    "{task_index:04}-{}.jsonl",
                    safe_task_name(&task.id)
                ))
            });
        let result = run_benchmark_task(
            &benchmark_config,
            task,
            workspace,
            event_log,
            &args.provider,
            started,
        )
        .await;
        let failed = result.status == BenchmarkStatus::Failed;
        run.results.push(result);
        if failed {
            run.failed += 1;
        } else {
            run.completed += 1;
        }
        run.skipped = run.total_tasks.saturating_sub(run.results.len());
        write_benchmark_results(&run, &artifacts, args.results.as_deref()).await?;
        if failed && args.fail_fast {
            break;
        }
    }
    run.finished = true;
    write_benchmark_results(&run, &artifacts, args.results.as_deref()).await?;
    let serialized = serde_json::to_string_pretty(&run).map_err(|error| error.to_string())?;
    println!("{serialized}");
    Ok(())
}

async fn write_benchmark_results(
    run: &BenchmarkRun,
    artifacts: &Path,
    extra_path: Option<&Path>,
) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(run).map_err(|error| error.to_string())?;
    // A rename keeps a reader from observing a partially written aggregate.
    let temporary = artifacts.join("results.json.tmp");
    tokio::fs::write(&temporary, format!("{serialized}\n"))
        .await
        .map_err(|error| format!("cannot write benchmark results: {error}"))?;
    tokio::fs::rename(temporary, artifacts.join("results.json"))
        .await
        .map_err(|error| format!("cannot publish benchmark results: {error}"))?;
    if let Some(path) = extra_path {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        tokio::fs::write(path, format!("{serialized}\n"))
            .await
            .map_err(|error| {
                format!("cannot write benchmark results {}: {error}", path.display())
            })?;
    }
    Ok(())
}

type BenchSession = Session<ConfiguredProvider, LocalExecutor, JsonlEventStore>;

async fn make_session(
    config: &mini_harness::config::HarnessConfig,
    session_id: SessionId,
    event_log: PathBuf,
    workspace: PathBuf,
    selection: &ProviderArgs,
) -> Result<BenchSession, String> {
    let store = Arc::new(JsonlEventStore::with_flush_mode(
        event_log,
        config.durable.flush,
    ));
    let mut registry = ToolRegistry::default();
    registry
        .register(ReadTool {
            max_bytes: config.execution.max_output_bytes,
        })
        .map_err(|error| error.to_string())?;
    registry
        .register(EditTool)
        .map_err(|error| error.to_string())?;
    registry
        .register(BashTool::with_default_timeout(
            std::time::Duration::from_millis(config.execution.default_timeout_ms),
        ))
        .map_err(|error| error.to_string())?;
    let (provider, model) = selection.resolve(config);
    let provider = match provider.as_str() {
        "mock" => ConfiguredProvider::mock(),
        "openai" => {
            let model =
                model.ok_or_else(|| "--model is required with --provider openai".to_owned())?;
            ConfiguredProvider::openai(model).map_err(|error| error.to_string())?
        }
        "deepseek" => {
            let model = model.unwrap_or_else(|| "deepseek-chat".to_owned());
            ConfiguredProvider::deepseek(model).map_err(|error| error.to_string())?
        }
        other => return Err(format!("unsupported provider `{other}`")),
    };
    Ok(Session::new_with_policy_and_config(
        session_id,
        Arc::new(provider),
        Arc::new(
            LocalExecutor::new(workspace)
                .with_process_limits(config.execution.max_concurrent_processes),
        ),
        Arc::new(registry),
        store,
        Arc::new(mini_harness::config::ConfiguredPolicy::new(
            config.permissions.clone(),
        )),
        config.agent_loop_config(),
    ))
}

async fn run_benchmark_task(
    config: &mini_harness::config::HarnessConfig,
    task: &mini_harness_benchmark::BenchmarkTask,
    workspace: PathBuf,
    event_log: PathBuf,
    provider: &ProviderArgs,
    started: Instant,
) -> BenchmarkResult {
    let workspace_display = workspace.display().to_string();
    let event_log_display = event_log.display().to_string();
    let elapsed = || started.elapsed().as_millis();
    let workspace_metadata = tokio::fs::metadata(&workspace).await;
    if !workspace_metadata
        .as_ref()
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return BenchmarkResult::failed(
            task,
            workspace_display,
            event_log_display,
            FailureCategory::Environment,
            format!(
                "benchmark workspace must already exist as a directory: {}",
                workspace.display()
            ),
            elapsed(),
        );
    }
    if let Some(parent) = event_log
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if let Err(error) = tokio::fs::create_dir_all(parent).await {
            return BenchmarkResult::failed(
                task,
                workspace_display,
                event_log_display,
                FailureCategory::Environment,
                format!("cannot create event-log directory: {error}"),
                elapsed(),
            );
        }
    }
    if let Err(error) = ensure_new_event_log(&event_log).await {
        return BenchmarkResult::failed(
            task,
            workspace_display,
            event_log_display,
            FailureCategory::Environment,
            error,
            elapsed(),
        );
    }
    let session_id = SessionId::new();
    let mut session = match make_session(
        config,
        session_id,
        event_log.clone(),
        workspace.clone(),
        provider,
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            return BenchmarkResult::failed(
                task,
                workspace_display,
                event_log_display,
                classify_cli_error(&error),
                error,
                elapsed(),
            );
        }
    };
    let (turn_id, text) = match session.start_turn(task.prompt.clone()).await {
        Ok(value) => value,
        Err(error) => {
            let mut result = BenchmarkResult::failed(
                task,
                workspace_display,
                event_log_display,
                classify_benchmark_error(&error, config.session.max_steps),
                error.to_string(),
                elapsed(),
            );
            result.session_id = Some(session.state().session_id.to_string());
            result.turn_id = session
                .state()
                .active_turn
                .as_ref()
                .map(|turn| turn.turn_id.to_string())
                .or(event_turn_id(&event_log).await);
            return result;
        }
    };
    BenchmarkResult::completed(
        task,
        workspace_display,
        event_log_display,
        session.state().session_id.to_string(),
        turn_id.to_string(),
        text,
        elapsed(),
    )
}

async fn event_turn_id(path: &Path) -> Option<String> {
    JsonlEventStore::new(path.to_path_buf())
        .read_from(EventSeq(1))
        .await
        .ok()?
        .into_iter()
        .rev()
        .find_map(|event| event.turn_id.map(|turn| turn.to_string()))
}

/// A benchmark task always starts a new session: fail before writing
/// anything if the log already exists.
async fn ensure_new_event_log(path: &Path) -> Result<(), String> {
    let store = JsonlEventStore::new(path.to_path_buf());
    store
        .repair_partial_tail()
        .await
        .map_err(|error| error.to_string())?;
    let events = store
        .read_from(EventSeq(1))
        .await
        .map_err(|error| error.to_string())?;
    if let Some(first) = events.first() {
        if !matches!(first.payload, EventPayload::SessionCreated) {
            return Err(format!(
                "event log {} already contains session {}",
                path.display(),
                first.session_id
            ));
        }
    }
    Ok(())
}

fn resolve_benchmark_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn safe_task_name(id: &str) -> String {
    let mut value = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        value.push_str("task");
    }
    value
}

fn classify_cli_error(error: &str) -> FailureCategory {
    if error.contains("provider") {
        FailureCategory::Provider
    } else if error.contains("workspace") || error.contains("event log") {
        FailureCategory::Environment
    } else {
        FailureCategory::Configuration
    }
}

fn classify_benchmark_error(
    error: &mini_harness::HarnessError,
    max_steps: usize,
) -> FailureCategory {
    use mini_harness::HarnessError;
    if let HarnessError::Provider(error) = error {
        if error.to_string() == format!("turn exceeded the {max_steps} step limit") {
            return FailureCategory::Budget;
        }
    }
    match error {
        HarnessError::Provider(_) => FailureCategory::Provider,
        HarnessError::Tool(_) => FailureCategory::Tool,
        HarnessError::Execution(_) => FailureCategory::Execution,
        HarnessError::Policy(_) => FailureCategory::Policy,
        HarnessError::ApprovalPending(_) => FailureCategory::ApprovalPending,
        HarnessError::Timeout => FailureCategory::Timeout,
        HarnessError::Cancelled => FailureCategory::Cancelled,
        HarnessError::Durable(_) => FailureCategory::Durable,
        HarnessError::Config(_) => FailureCategory::Configuration,
        _ => FailureCategory::Invariant,
    }
}
