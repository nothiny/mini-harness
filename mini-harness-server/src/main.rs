use clap::Parser;

/// JSONL 协议服务器: 在 stdin/stdout 上驱动 mini-harness 会话。
#[derive(Parser)]
#[command(name = "mini-harness-server", version)]
struct Args {
    /// Serve the JSONL protocol on stdin/stdout (the only mode).
    #[arg(long)]
    stdio: bool,
    /// Path to the TOML configuration file (defaults to ./mini-harness.toml).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if !args.stdio {
        eprintln!("usage: mini-harness-server --stdio");
        std::process::exit(2);
    }
    // stdout 归协议所有: 诊断一律写 stderr。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    let config_path = args.config.or_else(|| {
        std::path::Path::new(mini_harness::config::DEFAULT_CONFIG_FILE)
            .exists()
            .then(|| std::path::PathBuf::from(mini_harness::config::DEFAULT_CONFIG_FILE))
    });
    let config = match mini_harness::config::HarnessConfig::load(config_path.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = mini_harness_server::run_stdio(config).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
