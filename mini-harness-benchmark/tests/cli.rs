use std::process::Command;
use tempfile::tempdir;

#[test]
fn benchmark_runs_manifest_without_waiting_for_approval() {
    let directory = tempdir().unwrap();
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
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness-benchmark"))
        .current_dir(directory.path())
        .args([
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
    let directory = tempdir().unwrap();
    let manifest = directory.path().join("tasks.json");
    std::fs::write(
        &manifest,
        r#"{"tasks":[{"id":"missing","prompt":"hello","workspace":"missing"}]}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness-benchmark"))
        .current_dir(directory.path())
        .args([
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
