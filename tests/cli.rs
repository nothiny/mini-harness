use std::process::Command;
use tempfile::tempdir;

#[test]
fn demo_read_prints_the_runtime_event_flow() {
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["demo", "read", "src/main.rs"])
        .output()
        .expect("run mini-harness demo");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 4, "unexpected demo output: {stdout}");
    assert!(lines[0].starts_with("assistant tool call: read"));
    assert!(lines[1].starts_with("tool call: read"));
    assert!(lines[1].contains("src/main.rs"));
    assert!(lines[2].starts_with("tool result: "));
    assert!(lines[2].contains("fn main"));
    assert_eq!(lines[3], "assistant final: read complete");
}

#[test]
fn demo_read_json_is_an_explicit_machine_output_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["demo", "read", "src/main.rs", "--json"])
        .output()
        .expect("run JSON demo");

    assert!(output.status.success());
    let events: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(events.as_array().is_some_and(|events| !events.is_empty()));
}

#[test]
fn demo_rejects_absolute_paths_and_stays_inside_the_workspace() {
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["demo", "read", "/etc/hosts"])
        .output()
        .expect("run mini-harness demo");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("relative"),
        "expected a relative-path error, got: {stderr}"
    );
}

#[test]
fn bare_arguments_are_rejected_with_a_usage_hint() {
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["hello"])
        .output()
        .expect("run mini-harness");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected a subcommand error, got: {stderr}"
    );
    assert!(
        !stderr.contains("error: error:"),
        "doubled error prefix: {stderr}"
    );
}

#[test]
fn cli_run_resume_and_cancel_expose_session_identity() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let run = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .args([
            "run",
            "hello",
            "--event-log",
            log.to_str().unwrap(),
            "--workspace",
            directory.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    let session_id = value["session_id"].as_str().unwrap();
    let turn_id = value["turn_id"].as_str().unwrap();
    assert_eq!(value["text"], "mock response");
    assert_eq!(value["event_log"], log.to_string_lossy().as_ref());

    let resumed = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .args([
            "resume",
            session_id,
            "follow-up",
            "--event-log",
            log.to_str().unwrap(),
            "--workspace",
            directory.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_value: serde_json::Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(resumed_value["session_id"], session_id);
    assert_eq!(resumed_value["text"], "mock response");

    // A completed turn cannot be cancelled; the command reports a structured
    // failure instead of pretending a different process was stopped.
    let cancel = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .args([
            "cancel",
            session_id,
            turn_id,
            "--event-log",
            log.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!cancel.status.success());
    assert!(String::from_utf8_lossy(&cancel.stderr).contains("not active"));
}

#[test]
fn cli_requires_a_model_when_openai_provider_is_selected() {
    let directory = tempdir().unwrap();
    let log = directory.path().join("events.jsonl");
    let result = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .args([
            "run",
            "hello",
            "--provider",
            "openai",
            "--event-log",
            log.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--model is required"));
}

#[test]
fn config_file_supplies_provider_and_model_when_flags_are_omitted() {
    let directory = tempfile::tempdir().unwrap();
    // [model] names an openai model with no flags: the CLI must use the
    // config values (failing later on the missing env var proves the name
    // was resolved from config rather than the "--model is required" error).
    std::fs::write(
        directory.path().join("mini-harness.toml"),
        "[model]\nprovider = \"openai\"\nname = \"config-model-x\"\n",
    )
    .unwrap();
    let log = directory.path().join("events.jsonl");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(directory.path())
        .env_remove("OPENAI_API_KEY")
        .args(["run", "hello", "--event-log", log.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("OPENAI_API_KEY is not set"),
        "config [model] must supply provider+model: {stderr}"
    );
    assert!(
        !stderr.contains("--model is required"),
        "model name must come from config: {stderr}"
    );
}

#[test]
fn benchmark_runs_manifest_without_waiting_for_approval() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("task");
    std::fs::create_dir_all(&workspace).unwrap();
    let manifest = directory.path().join("tasks.toml");
    std::fs::write(
        &manifest,
        format!(
            "name = \"smoke\"\n[[tasks]]\nid = \"hello\"\nprompt = \"say hello\"\nworkspace = {:?}\n",
            workspace
        ),
    )
    .unwrap();
    let output_path = directory.path().join("results.json");
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(directory.path())
        .args([
            "benchmark",
            manifest.to_str().unwrap(),
            "--provider",
            "mock",
            "--artifacts",
            "artifacts",
            "--results",
            output_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["completed"], 1);
    assert_eq!(value["failed"], 0);
    assert_eq!(value["results"][0]["task_id"], "hello");
    assert_eq!(value["results"][0]["execution_status"], "succeeded");
    assert_eq!(value["results"][0]["task_result"], "not_verified");
    assert_eq!(value["finished"], true);
    assert_eq!(value["binary_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["effective_config"]["permissions"]["bash"], "allow");
    assert!(output_path.exists());
    let artifacts = std::path::Path::new(value["artifact_dir"].as_str().unwrap());
    assert!(artifacts.join("results.json").exists());
    assert!(artifacts.join("effective-config.toml").exists());
    let saved_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifacts.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(saved_manifest["tasks"][0]["prompt"], "say hello");
    let event_log = value["results"][0]["event_log"].as_str().unwrap();
    assert!(std::path::Path::new(event_log).exists());
}

#[test]
fn benchmark_preserves_failures_without_creating_missing_workspaces() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("tasks.json");
    std::fs::write(
        &manifest,
        r#"{"tasks":[{"id":"missing","prompt":"hello","workspace":"missing"}]}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(directory.path())
        .args([
            "benchmark",
            "--manifest",
            manifest.to_str().unwrap(),
            "--provider",
            "mock",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["completed"], 0);
    assert_eq!(value["failed"], 1);
    assert_eq!(value["results"][0]["execution_status"], "environment_error");
    assert!(!directory.path().join("missing").exists());
    assert!(
        std::path::Path::new(value["artifact_dir"].as_str().unwrap())
            .join("results.json")
            .exists()
    );
}
