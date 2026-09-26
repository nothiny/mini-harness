use std::{
    io::Write,
    process::{Command, Stdio},
};

use tempfile::tempdir;

fn run_tui(
    log: &std::path::Path,
    workspace: &std::path::Path,
    extra: &[String],
    input: &str,
) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mini-harness"));
    command
        .arg("tui")
        .args(extra)
        .args(["--event-log", log.to_str().expect("UTF-8 event log")])
        .args(["--workspace", workspace.to_str().expect("UTF-8 workspace")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn tui");
    child
        .stdin
        .take()
        .expect("tui stdin")
        .write_all(input.as_bytes())
        .expect("write tui input");
    let output = child.wait_with_output().expect("wait for tui");
    assert!(
        output.status.success(),
        "tui failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("tui stdout is UTF-8")
}

fn event_sequences(output: &str) -> Vec<u64> {
    output
        .lines()
        .filter_map(|line| line.split_once("event seq=").map(|(_, event)| event))
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(seq, _)| seq.parse::<u64>().ok())
        .collect()
}

#[test]
fn line_mode_tui_drives_multiple_turns_and_deduplicates_events() {
    let directory = tempdir().expect("temporary workspace");
    let log = directory.path().join("tui-events.jsonl");
    let output = run_tui(&log, directory.path(), &[], "one\ntwo\n");

    assert!(output.contains("session "));
    assert_eq!(output.matches("turn completed: mock response").count(), 2);
    let sequences = event_sequences(&output);
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(sequences.contains(&1));
    assert!(sequences.contains(&9));
    assert!(!output.contains("payload"));
    assert!(!output.contains("input="));
    assert!(!output.contains("output="));
}

#[test]
fn line_mode_tui_can_resume_and_redraw_history() {
    let directory = tempdir().expect("temporary workspace");
    let log = directory.path().join("resume-events.jsonl");
    let first = run_tui(&log, directory.path(), &[], "one\n");
    let session_id = first
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .and_then(|line| line.split_once(';'))
        .map(|(session, _)| session.to_owned())
        .expect("session id in tui header");

    let second = run_tui(
        &log,
        directory.path(),
        &["--resume-session".into(), session_id],
        "",
    );
    assert!(second.contains("type=session.created"));
    assert!(second.contains("type=turn.completed"));
    let sequences = event_sequences(&second);
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn line_mode_tui_accepts_provider_and_model_flags() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("provider-tui.jsonl");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tui",
            "--event-log",
            log.to_str().unwrap(),
            "--workspace",
            directory.path().to_str().unwrap(),
            "--provider",
            "mock",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("tui stdin")
                .write_all(b"flagged provider turn\n/quit\n")?;
            child.wait_with_output()
        })
        .expect("run flagged tui");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("turn completed: mock response"),
        "flagged provider must drive the turn: {stdout}"
    );
}
