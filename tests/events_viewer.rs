use std::process::Command;

use tempfile::tempdir;

#[test]
fn event_viewer_drives_jsonl_server_and_prints_deduplicated_events() {
    let directory = tempdir().expect("temporary workspace");
    let log = directory.path().join("events.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_mini-harness"))
        .args([
            "events",
            "hello",
            "--event-log",
            log.to_str().expect("event log path is UTF-8"),
            "--workspace",
            directory.path().to_str().expect("workspace path is UTF-8"),
        ])
        .output()
        .expect("run event viewer");

    assert!(
        output.status.success(),
        "event viewer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("viewer output is UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("type=session.created"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("type=user.input.recorded"))
    );
    assert!(lines.iter().any(|line| line.contains("type=turn.started")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("type=model.response.recorded"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("type=turn.completed"))
    );

    let sequences = lines
        .iter()
        .map(|line| {
            line.strip_prefix("event seq=")
                .and_then(|line| line.split_once(' '))
                .and_then(|(seq, _)| seq.parse::<u64>().ok())
                .expect("event viewer prints a sequence")
        })
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!stdout.contains("payload"));
    assert!(!stdout.contains("input="));
    assert!(!stdout.contains("output="));
    assert!(!stdout.contains("command="));
    assert!(log.exists());
}
