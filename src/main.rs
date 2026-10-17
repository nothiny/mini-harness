use clap::{Args, Parser, Subcommand, ValueEnum};
use mini_harness::{
    durable::{Event, EventPayload, EventStore, InMemoryEventStore, JsonlEventStore, reduce},
    executor::LocalExecutor,
    model::{ConfiguredProvider, MockProvider, MockResponse, ToolCall},
    policy::{AllowAllPolicy, DefaultPolicy, ToolPolicy},
    runtime::{EventSeq, SessionId, SessionState, ToolCallId, ToolName, TurnId, session::Session},
    tools::{BashTool, EditTool, ReadTool, ToolRegistry},
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

type CliComponents = (
    Arc<ConfiguredProvider>,
    Arc<LocalExecutor>,
    Arc<ToolRegistry>,
);

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Initializes stderr-only tracing.
///
/// stdout is reserved for structured output, so diagnostics must never go
/// there. The default level is quiet (`warn`); set
/// `RUST_LOG=mini_harness=debug` to inspect turns, tools and providers.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run() -> Result<(), String> {
    init_tracing();
    let cli = Cli::parse();
    let config_path = cli.config.clone().or_else(|| {
        Path::new(mini_harness::config::DEFAULT_CONFIG_FILE)
            .exists()
            .then(|| PathBuf::from(mini_harness::config::DEFAULT_CONFIG_FILE))
    });
    let config = mini_harness::config::HarnessConfig::load(config_path.as_deref())
        .map_err(|error| error.to_string())?;
    match cli.command {
        CliCommand::Run(args) => {
            run_cli_turn(
                &config,
                args.event_log,
                args.workspace,
                args.prompt,
                args.provider,
            )
            .await
        }
        CliCommand::Resume(args) => {
            run_cli_resume(
                &config,
                args.event_log,
                args.workspace,
                args.session_id,
                args.prompt,
                args.provider,
            )
            .await
        }
        CliCommand::Cancel(args) => run_cli_cancel(&config, args).await,
        CliCommand::Events(args) => run_events(&config, &args.reference, args.format).await,
        CliCommand::Inspect(args) => run_inspect(&config, &args.reference, args.format).await,
        CliCommand::Sessions(args) => run_sessions(&config, args.format).await,
        CliCommand::Demo(args) => run_demo(args).await,
        CliCommand::Config(_) => run_config_show(&config).await,
        CliCommand::Doctor(_) => run_doctor(&config).await,
    }
}

use std::path::Path;

#[derive(Parser)]
#[command(name = "mini-harness", version, about = "minimal agent harness")]
struct Cli {
    /// Path to the TOML configuration file. Defaults to ./mini-harness.toml
    /// when present; built-in defaults otherwise.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Run(RunArgs),
    Resume(ResumeArgs),
    Cancel(CancelArgs),
    Events(LogArgs),
    Inspect(LogArgs),
    Sessions(SessionsArgs),
    Demo(DemoArgs),
    Config(ConfigShowArgs),
    Doctor(DoctorArgs),
}

#[derive(Args)]
struct ConfigShowArgs {}

#[derive(Args)]
struct DoctorArgs {}

#[derive(Args)]
struct SessionsArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    format: OutputFormat,
}

/// Scripted MockProvider runs used to see the harness event flow offline.
///
/// Demos intentionally use an in-memory event log: nothing is persisted, and
/// they never contact a provider. Use `run` for a real durable session.
#[derive(Args)]
struct DemoArgs {
    #[command(subcommand)]
    command: DemoCommand,
}

#[derive(Subcommand)]
enum DemoCommand {
    /// One mock `read` tool call against a workspace file.
    Read(DemoPathArgs),
    /// One mock `edit` (before → after) followed by a `bash` verification.
    EditBash(DemoPathArgs),
}

#[derive(Args)]
struct DemoPathArgs {
    /// Path relative to the current working directory.
    path: String,
    /// Print the full durable event array as JSON instead of a summary.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    /// Redacted JSON suitable for scripts and local inspection.
    Json,
    /// Complete event details for explicitly trusted local workflows.
    RawJson,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProviderChoice {
    Mock,
    Openai,
    Deepseek,
}

#[derive(Clone, Args)]
struct ProviderArgs {
    /// Omit to fall back to the [model] configuration section (then "mock").
    #[arg(long, value_enum)]
    provider: Option<ProviderChoice>,
    #[arg(long, help = "model name used by the OpenAI/DeepSeek provider")]
    model: Option<String>,
}

impl ProviderArgs {
    /// Resolves the effective provider/model names: CLI flags override the
    /// `[model]` config section, which overrides the built-in defaults.
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

#[derive(Args)]
struct RunArgs {
    prompt: Vec<String>,
    /// Explicit event-log path; omit to use the durable layout
    /// `<durable.root>/sessions/<session-id>/events.jsonl`.
    #[arg(long)]
    event_log: Option<PathBuf>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[command(flatten)]
    provider: ProviderArgs,
}

#[derive(Args)]
struct ResumeArgs {
    session_id: SessionId,
    prompt: Vec<String>,
    #[arg(long)]
    event_log: Option<PathBuf>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[command(flatten)]
    provider: ProviderArgs,
}

#[derive(Args)]
struct LogArgs {
    /// Event-log path or a session id resolved against the durable layout.
    reference: String,
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    format: OutputFormat,
}

#[derive(Args)]
struct CancelArgs {
    session_id: SessionId,
    turn_id: TurnId,
    #[arg(long)]
    event_log: Option<PathBuf>,
    #[command(flatten)]
    provider: ProviderArgs,
}

/// `mini-harness config`: prints the effective configuration (no secrets).
async fn run_config_show(config: &mini_harness::config::HarnessConfig) -> Result<(), String> {
    println!("{}", config.to_toml().map_err(|error| error.to_string())?);
    Ok(())
}

/// `mini-harness doctor`: checks the local environment a session depends on.
async fn run_doctor(config: &mini_harness::config::HarnessConfig) -> Result<(), String> {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        let status = if ok { "ok" } else { "FAIL" };
        if !ok {
            failures += 1;
        }
        println!("{status}  {name}  {detail}");
    };

    let root = config.durable_root();
    let root_ok = std::fs::create_dir_all(&root).is_ok();
    let probe = root.join(".doctor-probe");
    let writable = root_ok && std::fs::write(&probe, b"probe").is_ok();
    let _ = std::fs::remove_file(&probe);
    check(
        "durable-root",
        writable,
        format!("{} writable", root.display()),
    );

    match config.model.provider.as_str() {
        "mock" => check("provider", true, "mock (offline)".into()),
        "openai" => {
            let key = std::env::var("OPENAI_API_KEY").is_ok();
            check("provider", key, "openai: OPENAI_API_KEY present".into());
        }
        "deepseek" => {
            let key = std::env::var("DEEPSEEK_API_KEY").is_ok();
            check("provider", key, "deepseek: DEEPSEEK_API_KEY present".into());
        }
        other => check("provider", false, format!("unknown provider `{other}`")),
    }

    let mut registry = ToolRegistry::default();
    let tools_ok = registry
        .register(ReadTool {
            max_bytes: config.execution.max_output_bytes,
        })
        .is_ok()
        && registry.register(EditTool).is_ok()
        && registry
            .register(BashTool::with_default_timeout(
                std::time::Duration::from_millis(config.execution.default_timeout_ms),
            ))
            .is_ok();
    check("tools", tools_ok, "read/edit/bash registered".into());

    #[cfg(unix)]
    check("platform", true, "unix process groups available".into());
    #[cfg(not(unix))]
    check(
        "platform",
        false,
        "process-group cleanup is unix-only; commands may leave children".into(),
    );

    if failures > 0 {
        return Err(format!("{failures} doctor check(s) failed"));
    }
    Ok(())
}

/// `mini-harness sessions`: lists sessions stored in the durable layout.
async fn run_sessions(
    config: &mini_harness::config::HarnessConfig,
    format: OutputFormat,
) -> Result<(), String> {
    let sessions_root = config.durable_root().join("sessions");
    let mut entries = Vec::new();
    let mut dir = match tokio::fs::read_dir(&sessions_root).await {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            print_entries(&entries, format)?;
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };
    while let Some(entry) = dir.next_entry().await.map_err(|error| error.to_string())? {
        let log = entry.path().join("events.jsonl");
        if !log.exists() {
            continue;
        }
        let store = JsonlEventStore::new(log);
        let Ok(events) = store.read_from(EventSeq(1)).await else {
            continue;
        };
        let Some(session_id) = events.first().map(|event| event.session_id) else {
            continue;
        };
        let summary = match replay_state(&events, session_id) {
            Ok(state) => serde_json::json!({
                "session_id": session_id,
                "last_seq": state.last_seq.0,
                "status": format!("{:?}", state.status).to_lowercase(),
                "active_turn": state.active_turn.as_ref().map(|turn| turn.turn_id.to_string()),
                "history_items": state.history.len(),
            }),
            Err(error) => serde_json::json!({
                "session_id": session_id,
                "last_seq": events.len(),
                "status": "unreadable",
                "error": error.to_string(),
            }),
        };
        entries.push(summary);
    }
    entries.sort_by_key(|entry| {
        entry["session_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default()
    });
    print_entries(&entries, format)
}

fn print_entries(entries: &[serde_json::Value], format: OutputFormat) -> Result<(), String> {
    match format {
        OutputFormat::Json | OutputFormat::RawJson => println!(
            "{}",
            serde_json::to_string_pretty(entries).map_err(|error| error.to_string())?
        ),
    }
    Ok(())
}

/// 从事件日志重建状态（完整重放）。
fn replay_state(
    events: &[Event],
    session_id: SessionId,
) -> Result<SessionState, mini_harness::HarnessError> {
    let mut state = SessionState::new(session_id);
    for event in events {
        reduce(&mut state, event)?;
    }
    Ok(state)
}

/// `mini-harness inspect`: 重放事件日志并打印会话状态。
async fn run_inspect(
    config: &mini_harness::config::HarnessConfig,
    reference: &str,
    format: OutputFormat,
) -> Result<(), String> {
    let store = open_referenced_log(config, reference).await?;
    let events = store
        .read_from(EventSeq(1))
        .await
        .map_err(|error| error.to_string())?;
    let session_id = events
        .first()
        .map(|event| event.session_id)
        .ok_or_else(|| "event log is empty".to_string())?;
    let state = replay_state(&events, session_id).map_err(|error| error.to_string())?;
    let value = serde_json::to_value(&state).map_err(|error| error.to_string())?;
    let value = match format {
        OutputFormat::Json => redact_value(&value),
        OutputFormat::RawJson => value,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
    );
    Ok(())
}

/// `mini-harness events`: 查看会话的事件日志（每个事实一行摘要）。
async fn run_events(
    config: &mini_harness::config::HarnessConfig,
    reference: &str,
    format: OutputFormat,
) -> Result<(), String> {
    let store = open_referenced_log(config, reference).await?;
    let events = store
        .read_from(EventSeq(1))
        .await
        .map_err(|error| error.to_string())?;
    match format {
        OutputFormat::RawJson => {
            for event in &events {
                println!(
                    "{}",
                    serde_json::to_string(event).map_err(|error| error.to_string())?
                );
            }
        }
        OutputFormat::Json => print_events(&events),
    }
    Ok(())
}

/// Opens the event log referenced either by path or by session id (durable
/// layout) after repairing any crash-truncated tail.
async fn open_referenced_log(
    config: &mini_harness::config::HarnessConfig,
    reference: &str,
) -> Result<JsonlEventStore, String> {
    let path = mini_harness::config::resolve_event_log(config, reference);
    let store = JsonlEventStore::new(path);
    store
        .repair_partial_tail()
        .await
        .map_err(|error| error.to_string())?;
    Ok(store)
}

fn print_events(events: &[Event]) {
    for event in events {
        let note = match &event.payload {
            EventPayload::SessionCreated => "session created".to_string(),
            EventPayload::UserInputRecorded { input } => {
                format!("user: {}", truncate_for_display(&input.0))
            }
            EventPayload::TurnStarted => "turn started".to_string(),
            EventPayload::ModelResponseRecorded { text } => {
                format!("assistant: {}", truncate_for_display(&text.0))
            }
            EventPayload::ProviderContinuationUpdated { .. } => String::new(),
            EventPayload::ProviderAttemptRecorded {
                attempt, outcome, ..
            } => {
                format!("provider attempt {attempt}: {outcome}")
            }
            EventPayload::ToolRequested {
                name,
                input,
                requires_approval,
                ..
            } => format!(
                "tool call: {} {}{}",
                name.0,
                summarize_tool_input(input),
                if *requires_approval {
                    " (approval required)"
                } else {
                    ""
                }
            ),
            EventPayload::ToolApprovalResponded { approved, .. } => {
                format!(
                    "approval: {}",
                    if *approved { "approved" } else { "denied" }
                )
            }
            EventPayload::ToolCompleted {
                outcome,
                duration_ms,
                ..
            } => match outcome {
                mini_harness::durable::ToolOutcome::Succeeded { result } => format!(
                    "tool result ({}ms): {}",
                    duration_ms,
                    truncate_for_display(&result.0)
                ),
                mini_harness::durable::ToolOutcome::Failed { error }
                | mini_harness::durable::ToolOutcome::Rejected { error } => {
                    format!("tool error: {}", truncate_for_display(&error.to_string()))
                }
            },
            EventPayload::ToolOutcomeUnknown { reason, .. } => {
                format!("tool outcome unknown: {reason}")
            }
            EventPayload::TurnCompleted { text } => {
                format!("turn completed: {}", truncate_for_display(&text.0))
            }
            EventPayload::TurnFailed { error } => format!("turn failed: {error}"),
            EventPayload::TurnCancelRequested => "cancel requested".to_string(),
            EventPayload::TurnCancelled => "turn cancelled".to_string(),
            EventPayload::TurnTimedOut => "turn timed out".to_string(),
        };
        println!(
            "{:>4}  {:>26}  {}",
            event.seq.0.to_string(),
            event.payload.kind(),
            note
        );
    }
}

const MAX_DISPLAY_BYTES: usize = 16 * 1024;

fn summarize_tool_input(input: &Value) -> String {
    let redacted = redact_value(input);
    serde_json::to_string(&redacted)
        .map(|value| truncate_for_display(&value))
        .unwrap_or_else(|_| "<unserializable input>".into())
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    let sensitive = [
                        "command",
                        "old_text",
                        "new_text",
                        "input",
                        "result",
                        "token",
                        "password",
                        "secret",
                        "api_key",
                        "authorization",
                        "cookie",
                        "headers",
                        "environment",
                    ]
                    .iter()
                    .any(|part| lower.contains(part));
                    (
                        key.clone(),
                        if sensitive {
                            Value::String("<redacted>".into())
                        } else {
                            redact_value(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_value).collect()),
        _ => value.clone(),
    }
}

fn truncate_for_display(value: &str) -> String {
    if value.len() <= MAX_DISPLAY_BYTES {
        return value.into();
    }
    let mut end = MAX_DISPLAY_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… <truncated>", &value[..end])
}

/// Drives one scripted mock turn and prints either a human summary or the
/// complete event array. See `DemoArgs` for the persistence boundary.
async fn run_demo(args: DemoArgs) -> Result<(), String> {
    enum DemoMode {
        Read,
        EditBash,
    }
    let (mode, path, json) = match args.command {
        DemoCommand::Read(args) => (DemoMode::Read, args.path, args.json),
        DemoCommand::EditBash(args) => (DemoMode::EditBash, args.path, args.json),
    };
    if path.is_empty() || !Path::new(&path).is_relative() {
        return Err("demo path must be relative to the current working directory".into());
    }
    let cat_command = |path: &str| {
        #[cfg(windows)]
        {
            format!("type \"{}\"", path.replace('"', "\"\""))
        }
        #[cfg(not(windows))]
        {
            format!("cat '{}'", path.replace('\'', "'\\''"))
        }
    };
    let script = match &mode {
        DemoMode::Read => vec![
            MockResponse::ToolCall(ToolCall {
                call_id: ToolCallId::new(),
                name: ToolName("read".into()),
                input: serde_json::json!({"path": path}),
            }),
            MockResponse::Text("read complete".into()),
        ],
        DemoMode::EditBash => vec![
            MockResponse::ToolCall(ToolCall {
                call_id: ToolCallId::new(),
                name: ToolName("edit".into()),
                input: serde_json::json!({
                    "path": path,
                    "old_text": "before",
                    "new_text": "after"
                }),
            }),
            MockResponse::ToolCall(ToolCall {
                call_id: ToolCallId::new(),
                name: ToolName("bash".into()),
                input: serde_json::json!({"command": cat_command(&path)}),
            }),
            MockResponse::Text("edit and bash complete".into()),
        ],
    };
    let provider = Arc::new(MockProvider::new(script));
    let mut registry = ToolRegistry::default();
    registry
        .register(ReadTool { max_bytes: 200_000 })
        .map_err(|error| error.to_string())?;
    registry
        .register(EditTool)
        .map_err(|error| error.to_string())?;
    registry
        .register(BashTool::default())
        .map_err(|error| error.to_string())?;
    let store = Arc::new(InMemoryEventStore::default());
    let policy: Arc<dyn ToolPolicy> = match &mode {
        DemoMode::EditBash => Arc::new(AllowAllPolicy),
        DemoMode::Read => Arc::new(DefaultPolicy),
    };
    let mut session = Session::new_with_policy(
        provider,
        Arc::new(LocalExecutor::new(
            std::env::current_dir().map_err(|error| error.to_string())?,
        )),
        Arc::new(registry),
        Arc::clone(&store),
        policy,
    );
    let input = match &mode {
        DemoMode::Read => format!("read {path}"),
        DemoMode::EditBash => format!("edit and verify {path}"),
    };
    let outcome = session.start_turn(input).await;
    let events = store
        .read_from(EventSeq(1))
        .await
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&events).map_err(|error| error.to_string())?
        );
    } else {
        print_events(&events);
    }
    outcome.map_err(|error| error.to_string())?;
    Ok(())
}

async fn run_cli_turn(
    config: &mini_harness::config::HarnessConfig,
    event_log: Option<PathBuf>,
    workspace: Option<PathBuf>,
    prompt: Vec<String>,
    provider: ProviderArgs,
) -> Result<(), String> {
    let workspace = workspace
        .or_else(|| config.session.workspace.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let prompt = prompt.join(" ");
    if prompt.is_empty() {
        return Err("usage: mini-harness run <prompt>".into());
    }
    // No explicit path: derive it from the durable layout so every session
    // lives under <durable.root>/sessions/<session-id>/.
    let session_id = SessionId::new();
    let event_log = event_log.unwrap_or_else(|| config.session_event_log(session_id));
    ensure_new_event_log(&event_log).await?;
    let event_log_display = event_log.display().to_string();
    let mut session = make_cli_session(config, session_id, event_log, workspace, provider).await?;
    let (turn_id, text) = session
        .start_turn(prompt)
        .await
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::json!({
            "session_id": session.state().session_id,
            "turn_id": turn_id,
            "text": text,
            "event_log": event_log_display,
        })
    );
    Ok(())
}

async fn run_cli_resume(
    config: &mini_harness::config::HarnessConfig,
    event_log: Option<PathBuf>,
    workspace: Option<PathBuf>,
    session_id: SessionId,
    prompt: Vec<String>,
    provider: ProviderArgs,
) -> Result<(), String> {
    let workspace = workspace
        .or_else(|| config.session.workspace.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let event_log = event_log.unwrap_or_else(|| config.session_event_log(session_id));
    let event_log_display = event_log.display().to_string();
    let store = Arc::new(JsonlEventStore::with_flush_mode(
        event_log,
        config.durable.flush,
    ));
    store
        .repair_partial_tail()
        .await
        .map_err(|error| error.to_string())?;
    let (provider, executor, tools) = cli_components(config, workspace, &provider)?;
    let mut session = Session::resume_with_policy(
        provider,
        executor,
        tools,
        store,
        session_id,
        Arc::new(mini_harness::config::ConfiguredPolicy::new(
            config.permissions.clone(),
        )),
        config.agent_loop_config(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let prompt = prompt.join(" ");
    if prompt.is_empty() {
        println!(
            "{}",
            serde_json::json!({
                "session_id": session_id,
                "resumed": true,
                "event_log": event_log_display,
            })
        );
        return Ok(());
    }
    let (turn_id, text) = session
        .start_turn(prompt)
        .await
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::json!({
            "session_id": session.state().session_id,
            "turn_id": turn_id,
            "text": text,
            "event_log": event_log_display,
        })
    );
    Ok(())
}

async fn run_cli_cancel(
    config: &mini_harness::config::HarnessConfig,
    args: CancelArgs,
) -> Result<(), String> {
    let workspace = config
        .session
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let event_log = args
        .event_log
        .unwrap_or_else(|| config.session_event_log(args.session_id));
    let store = Arc::new(JsonlEventStore::with_flush_mode(
        event_log,
        config.durable.flush,
    ));
    store
        .repair_partial_tail()
        .await
        .map_err(|error| error.to_string())?;
    let (provider, executor, tools) = cli_components(config, workspace, &args.provider)?;
    let mut session = Session::resume_with_policy(
        provider,
        executor,
        tools,
        store,
        args.session_id,
        Arc::new(mini_harness::config::ConfiguredPolicy::new(
            config.permissions.clone(),
        )),
        config.agent_loop_config(),
    )
    .await
    .map_err(|error| error.to_string())?;
    // `cancel` may run in a different process from `run`. Persisting the
    // terminal event is the cross-process handshake: the runner observes the
    // changed log before its next append and stops instead of writing over it.
    session
        .cancel_turn(args.turn_id)
        .await
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::json!({
            "session_id": args.session_id,
            "turn_id": args.turn_id,
            "cancelled": true,
            "cancel_requested": true,
            "durable": true,
        })
    );
    Ok(())
}

/// A `run` command always starts a new session. Reusing an existing log would
/// mix two session ids in one append-only stream, so fail before starting any
/// work and make the user choose a new path (or use `resume`).
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
        return Err(format!(
            "event log {} already contains session {}; use `resume` or choose another --event-log path",
            path.display(),
            first.session_id
        ));
    }
    Ok(())
}

async fn make_cli_session(
    config: &mini_harness::config::HarnessConfig,
    session_id: SessionId,
    event_log: PathBuf,
    workspace: PathBuf,
    selection: ProviderArgs,
) -> Result<Session<ConfiguredProvider, LocalExecutor, JsonlEventStore>, String> {
    let store = Arc::new(JsonlEventStore::with_flush_mode(
        event_log,
        config.durable.flush,
    ));
    let (provider, executor, tools) = cli_components(config, workspace, &selection)?;
    Ok(Session::new_with_policy_and_config(
        session_id,
        provider,
        executor,
        tools,
        store,
        Arc::new(mini_harness::config::ConfiguredPolicy::new(
            config.permissions.clone(),
        )),
        config.agent_loop_config(),
    ))
}

fn cli_components(
    config: &mini_harness::config::HarnessConfig,
    workspace: PathBuf,
    selection: &ProviderArgs,
) -> Result<CliComponents, String> {
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
    let executor = LocalExecutor::new(workspace)
        .with_process_limits(config.execution.max_concurrent_processes);
    Ok((Arc::new(provider), Arc::new(executor), Arc::new(registry)))
}
