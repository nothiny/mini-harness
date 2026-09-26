use crate::{
    error::ProcessError,
    executor::{ProcessOutput, ProcessRequest, ProcessResult},
};
use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

/// Runs a shell command in the supplied workspace and collects bounded output.
///
/// The child is placed in its own process group on Unix so cancellation and
/// timeout also terminate descendants. Readers continue draining both pipes
/// after their retained-output limits are reached, preventing a child from
/// blocking on a full pipe.
pub async fn run_process(
    root: &Path,
    request: ProcessRequest,
    cancel: CancellationToken,
) -> Result<ProcessResult, ProcessError> {
    if cancel.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let root =
        tokio::fs::canonicalize(root)
            .await
            .map_err(|error| ProcessError::WorkspaceRoot {
                message: error.to_string(),
            })?;
    let metadata =
        tokio::fs::metadata(&root)
            .await
            .map_err(|error| ProcessError::WorkspaceRoot {
                message: error.to_string(),
            })?;
    if !metadata.is_dir() {
        return Err(ProcessError::WorkspaceRoot {
            message: "workspace root is not a directory".into(),
        });
    }
    let mut command = shell_command(&request.command);
    command
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| ProcessError::Spawn {
        message: error.to_string(),
    })?;
    tracing::debug!(
        target: "mini_harness::process",
        pid = ?child.id(),
        command = %truncate_for_log(&request.command),
        timeout_ms = request.timeout.as_millis() as u64,
        "spawning workspace process"
    );
    let process_id = child.id();
    let mut process_group = ProcessGroupGuard::new(process_id);
    let deadline = Instant::now() + request.timeout;

    let stdout = child.stdout.take().ok_or_else(|| ProcessError::Io {
        message: "child stdout was not piped".into(),
    });
    let stderr = child.stderr.take().ok_or_else(|| ProcessError::Io {
        message: "child stderr was not piped".into(),
    });
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => {
            terminate_process(&mut child, process_id).await;
            let _ = child.wait().await;
            return Err(error);
        }
    };

    let combined = Arc::new(AtomicUsize::new(0));
    let mut stdout_task = tokio::spawn(collect_output(
        stdout,
        request.max_stdout_bytes.0,
        request.max_combined_bytes.0,
        Arc::clone(&combined),
    ));
    let mut stderr_task = tokio::spawn(collect_output(
        stderr,
        request.max_stderr_bytes.0,
        request.max_combined_bytes.0,
        combined,
    ));

    enum WaitOutcome {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
        WaitError(String),
    }

    let mut wait = Box::pin(child.wait());
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => WaitOutcome::Cancelled,
        _ = sleep_until(deadline) => WaitOutcome::TimedOut,
        status = &mut wait => match status {
            Ok(status) => WaitOutcome::Exited(status),
            Err(error) => WaitOutcome::WaitError(error.to_string()),
        },
    };
    drop(wait);

    let (status, timed_out) = match outcome {
        WaitOutcome::Exited(status) => (status, false),
        WaitOutcome::TimedOut => {
            terminate_process(&mut child, process_id).await;
            let status = match child.wait().await {
                Ok(status) => status,
                Err(error) => {
                    let _ = join_outputs(&mut stdout_task, &mut stderr_task).await;
                    return Err(ProcessError::Io {
                        message: error.to_string(),
                    });
                }
            };
            (status, true)
        }
        WaitOutcome::Cancelled => {
            terminate_process(&mut child, process_id).await;
            let _ = child.wait().await;
            let _ = finish_output_tasks(&mut stdout_task, &mut stderr_task).await;
            process_group.disarm();
            return Err(ProcessError::Cancelled);
        }
        WaitOutcome::WaitError(message) => {
            terminate_process(&mut child, process_id).await;
            let _ = child.wait().await;
            let _ = finish_output_tasks(&mut stdout_task, &mut stderr_task).await;
            process_group.disarm();
            return Err(ProcessError::Io { message });
        }
    };

    let output = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProcessOutputControl::Cancelled),
        _ = sleep_until(deadline) => Err(ProcessOutputControl::TimedOut),
        result = join_outputs(&mut stdout_task, &mut stderr_task) => result.map_err(ProcessOutputControl::Error),
    };
    let (stdout, stderr) = match output {
        Ok(output) => output,
        Err(ProcessOutputControl::Cancelled) => {
            terminate_process(&mut child, process_id).await;
            let _ = finish_output_tasks(&mut stdout_task, &mut stderr_task).await;
            process_group.disarm();
            return Err(ProcessError::Cancelled);
        }
        Err(ProcessOutputControl::TimedOut) => {
            terminate_process(&mut child, process_id).await;
            let output = finish_output_tasks(&mut stdout_task, &mut stderr_task)
                .await
                .unwrap_or_else(|_| (CollectedOutput::empty(), CollectedOutput::empty()));
            process_group.disarm();
            return Ok(ProcessResult {
                stdout: output.0.into_process_output(),
                stderr: output.1.into_process_output(),
                exit_code: status.code(),
                signal: signal_for(&status),
                timed_out: true,
            });
        }
        Err(ProcessOutputControl::Error(error)) => return Err(error),
    };
    process_group.disarm();
    tracing::debug!(
        target: "mini_harness::process",
        exit_code = status.code(),
        timed_out,
        stdout_original_bytes = stdout.original_bytes,
        stderr_original_bytes = stderr.original_bytes,
        "workspace process finished"
    );
    Ok(ProcessResult {
        stdout: stdout.into_process_output(),
        stderr: stderr.into_process_output(),
        exit_code: status.code(),
        signal: signal_for(&status),
        timed_out,
    })
}

enum ProcessOutputControl {
    Cancelled,
    TimedOut,
    Error(ProcessError),
}

struct ProcessGroupGuard {
    process_id: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(process_id: Option<u32>) -> Self {
        Self {
            process_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.process_id.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: the process was started in its own process group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(pid) = self.process_id {
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .status();
        }
    }
}

/// Stateless process executor facade for callers that prefer an associated API.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessExecutor;

impl ProcessExecutor {
    pub async fn run_process(
        root: &Path,
        request: ProcessRequest,
        cancel: CancellationToken,
    ) -> Result<ProcessResult, ProcessError> {
        run_process(root, request, cancel).await
    }
}

struct CollectedOutput {
    bytes: Vec<u8>,
    original_bytes: u64,
}

impl CollectedOutput {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            original_bytes: 0,
        }
    }

    fn into_process_output(self) -> ProcessOutput {
        let valid_len = match std::str::from_utf8(&self.bytes) {
            Ok(_) => self.bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => self.bytes.len(),
        };
        let text = String::from_utf8_lossy(&self.bytes[..valid_len]).into_owned();
        ProcessOutput {
            text,
            truncated: self.original_bytes > valid_len as u64 || valid_len < self.bytes.len(),
            original_bytes: self.original_bytes,
            retained_bytes: valid_len,
        }
    }
}

/// Command text is model-proposed and may contain anything; keep log lines
/// bounded and never dump multi-kilobyte commands into diagnostics.
fn truncate_for_log(value: &str) -> String {
    const MAX_LOG_COMMAND_CHARS: usize = 256;
    let mut truncated: String = value.chars().take(MAX_LOG_COMMAND_CHARS).collect();
    if value.chars().count() > MAX_LOG_COMMAND_CHARS {
        truncated.push('…');
    }
    truncated
}

#[cfg(unix)]
fn signal_for(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn signal_for(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

async fn collect_output<R>(
    mut reader: R,
    stream_limit: usize,
    combined_limit: usize,
    combined: Arc<AtomicUsize>,
) -> Result<CollectedOutput, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(stream_limit.min(8192));
    let mut original_bytes = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        original_bytes = original_bytes.saturating_add(read as u64);
        let stream_remaining = stream_limit.saturating_sub(bytes.len());
        if stream_remaining == 0 {
            continue;
        }
        let retained = reserve_combined(&combined, combined_limit, read.min(stream_remaining));
        if retained != 0 {
            bytes.extend_from_slice(&buffer[..retained]);
        }
    }
    Ok(CollectedOutput {
        bytes,
        original_bytes,
    })
}

fn reserve_combined(counter: &AtomicUsize, limit: usize, requested: usize) -> usize {
    if requested == 0 {
        return 0;
    }
    loop {
        let current = counter.load(Ordering::Relaxed);
        if current >= limit {
            return 0;
        }
        let allowed = requested.min(limit - current);
        if counter
            .compare_exchange_weak(
                current,
                current + allowed,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return allowed;
        }
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut shell = Command::new("cmd");
        shell.args(["/C", command]);
        shell
    }
    #[cfg(not(windows))]
    {
        let mut shell = Command::new("sh");
        shell.args(["-c", command]);
        shell
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command
            .as_std_mut()
            .creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

async fn terminate_process(child: &mut Child, process_id: Option<u32>) {
    tracing::debug!(target: "mini_harness::process", pid = ?process_id, "terminating process group");
    #[cfg(unix)]
    {
        if let Some(pid) = process_id.or_else(|| child.id()) {
            if let Ok(pid) = i32::try_from(pid) {
                // SAFETY: `pid` is the process group created for this child.
                let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
                if result == 0 {
                    return;
                }
            }
        }
        let _ = child.start_kill();
    }
    #[cfg(windows)]
    {
        if let Some(pid) = process_id.or_else(|| child.id()) {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .status()
                .await;
        }
        let _ = child.start_kill();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.start_kill();
    }
}

async fn join_outputs(
    stdout: &mut tokio::task::JoinHandle<Result<CollectedOutput, std::io::Error>>,
    stderr: &mut tokio::task::JoinHandle<Result<CollectedOutput, std::io::Error>>,
) -> Result<(CollectedOutput, CollectedOutput), ProcessError> {
    let (stdout, stderr) = tokio::join!(stdout, stderr);
    let stdout = stdout
        .map_err(|error| ProcessError::Io {
            message: format!("stdout reader task failed: {error}"),
        })?
        .map_err(|error| ProcessError::Io {
            message: format!("stdout read failed: {error}"),
        })?;
    let stderr = stderr
        .map_err(|error| ProcessError::Io {
            message: format!("stderr reader task failed: {error}"),
        })?
        .map_err(|error| ProcessError::Io {
            message: format!("stderr read failed: {error}"),
        })?;
    Ok((stdout, stderr))
}

async fn finish_output_tasks(
    stdout: &mut tokio::task::JoinHandle<Result<CollectedOutput, std::io::Error>>,
    stderr: &mut tokio::task::JoinHandle<Result<CollectedOutput, std::io::Error>>,
) -> Result<(CollectedOutput, CollectedOutput), ProcessError> {
    match tokio::time::timeout(Duration::from_secs(1), join_outputs(stdout, stderr)).await {
        Ok(result) => result,
        Err(_) => {
            stdout.abort();
            stderr.abort();
            let _ = stdout.await;
            let _ = stderr.await;
            Err(ProcessError::Io {
                message: "output readers did not stop after process termination".into(),
            })
        }
    }
}
