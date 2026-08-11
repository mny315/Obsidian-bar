use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{OnceLock, mpsc},
    thread,
    time::Duration,
};

const PROCESS_TERMINATION_GRACE: Duration = Duration::from_millis(250);
const PIPE_FINISH_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const OUTPUT_TRUNCATED_MARKER: &[u8] = b"\n[output truncated]\n";

static KILL: ExternalProgram = ExternalProgram::new(
    "OBSIDIAN_BAR_KILL_BIN",
    option_env!("OBSIDIAN_BAR_KILL_BIN"),
    "kill",
);

pub(super) struct ExternalProgram {
    runtime_variable: &'static str,
    build_time_value: Option<&'static str>,
    fallback: &'static str,
    resolved: OnceLock<OsString>,
}

impl ExternalProgram {
    pub(super) const fn new(
        runtime_variable: &'static str,
        build_time_value: Option<&'static str>,
        fallback: &'static str,
    ) -> Self {
        Self {
            runtime_variable,
            build_time_value,
            fallback,
            resolved: OnceLock::new(),
        }
    }

    pub(super) fn get(&self) -> &OsStr {
        self.resolved
            .get_or_init(|| {
                env::var_os(self.runtime_variable)
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        self.build_time_value
                            .filter(|value| !value.is_empty())
                            .map(OsString::from)
                    })
                    .unwrap_or_else(|| OsString::from(self.fallback))
            })
            .as_os_str()
    }
}

enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
    Failed(io::Error),
}

pub(super) enum StatusError {
    Io(io::Error),
    TimedOut,
    Failed,
}

pub(super) fn output(
    program: impl AsRef<OsStr>,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let output = run(program, args, timeout, true)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(super) fn status(
    program: impl AsRef<OsStr>,
    args: &[&str],
    timeout: Duration,
) -> Result<(), String> {
    run(program, args, timeout, false).map(|_| ())
}

pub(super) fn status_inherited<S>(
    program: impl AsRef<OsStr>,
    args: &[S],
    timeout: Duration,
) -> Result<(), StatusError>
where
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).process_group(0);
    let child = command.spawn().map_err(StatusError::Io)?;

    match wait_for_exit(child, timeout) {
        WaitOutcome::Exited(status) if status.success() => Ok(()),
        WaitOutcome::Exited(_) => Err(StatusError::Failed),
        WaitOutcome::TimedOut => Err(StatusError::TimedOut),
        WaitOutcome::Failed(error) => Err(StatusError::Io(error)),
    }
}

fn run(
    program: impl AsRef<OsStr>,
    args: &[&str],
    timeout: Duration,
    capture_stdout: bool,
) -> Result<Output, String> {
    let program = program.as_ref();
    let display_name = program.to_string_lossy();
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start {display_name}: {error}"))?;

    let process_group = child.id();
    let stdout_reader = child.stdout.take().map(read_pipe);
    let stderr_reader = child.stderr.take().map(read_pipe);
    let outcome = wait_for_exit(child, timeout);

    let (status, stdout, stderr) = match outcome {
        WaitOutcome::Exited(status) => {
            let stdout = finish_pipe(stdout_reader, display_name.as_ref(), "stdout");
            let stderr = finish_pipe(stderr_reader, display_name.as_ref(), "stderr");
            match (stdout, stderr) {
                (Ok(stdout), Ok(stderr)) => (status, stdout, stderr),
                (stdout, stderr) => {
                    terminate_process_group(process_group, "-KILL");
                    return Err(stdout
                        .err()
                        .or_else(|| stderr.err())
                        .unwrap_or_else(|| format!("failed to drain {display_name} output")));
                }
            }
        }
        WaitOutcome::TimedOut => {
            let _ = finish_pipe(stdout_reader, display_name.as_ref(), "stdout");
            let _ = finish_pipe(stderr_reader, display_name.as_ref(), "stderr");
            return Err(format!(
                "{display_name} timed out after {} ms",
                timeout.as_millis()
            ));
        }
        WaitOutcome::Failed(error) => {
            let _ = finish_pipe(stdout_reader, display_name.as_ref(), "stdout");
            let _ = finish_pipe(stderr_reader, display_name.as_ref(), "stderr");
            return Err(format!("failed to wait for {display_name}: {error}"));
        }
    };

    let output = Output {
        status,
        stdout,
        stderr,
    };
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if stderr.is_empty() {
        format!("{display_name} exited with {}", output.status)
    } else {
        format!("{display_name}: {stderr}")
    })
}

fn wait_for_exit(mut child: Child, timeout: Duration) -> WaitOutcome {
    let process_group = child.id();
    let (sender, receiver) = mpsc::sync_channel(1);
    let waiter = thread::Builder::new()
        .name(format!("process-wait-{process_group}"))
        .spawn(move || {
            let _ = sender.send(child.wait());
        });

    if let Err(error) = waiter {
        terminate_process_group(process_group, "-KILL");
        return WaitOutcome::Failed(error);
    }

    match receiver.recv_timeout(timeout) {
        Ok(Ok(status)) => WaitOutcome::Exited(status),
        Ok(Err(error)) => {
            terminate_process_group(process_group, "-KILL");
            WaitOutcome::Failed(error)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            terminate_process_group(process_group, "-KILL");
            WaitOutcome::Failed(io::Error::other(
                "process waiter stopped before reporting an exit status",
            ))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            terminate_process_group(process_group, "-TERM");
            match receiver.recv_timeout(PROCESS_TERMINATION_GRACE) {
                Ok(_) => {
                    // The leader exited after TERM; kill any descendants that kept
                    // the process group alive and still report the original timeout.
                    terminate_process_group(process_group, "-KILL");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    terminate_process_group(process_group, "-KILL");
                    let _ = receiver.recv_timeout(PIPE_FINISH_TIMEOUT);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
            WaitOutcome::TimedOut
        }
    }
}

fn terminate_process_group(process_group_id: u32, signal: &str) {
    let process_group = format!("-{process_group_id}");
    let _ = Command::new(KILL.get())
        .args([signal, "--", process_group.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn read_pipe<R>(mut pipe: R) -> mpsc::Receiver<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker_sender = sender.clone();
    let spawn_result = thread::Builder::new()
        .name("process-output-reader".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 8192];
            let mut truncated = false;
            let result = loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break Ok(()),
                    Ok(read) => {
                        let remaining = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
                        let kept = read.min(remaining);
                        bytes.extend_from_slice(&buffer[..kept]);
                        truncated |= kept < read;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => break Err(error),
                }
            };

            let result = result.map(|()| {
                if truncated {
                    bytes.extend_from_slice(OUTPUT_TRUNCATED_MARKER);
                }
                bytes
            });
            let _ = worker_sender.send(result);
        });
    if let Err(error) = spawn_result {
        let _ = sender.send(Err(error));
    }
    receiver
}

fn finish_pipe(
    reader: Option<mpsc::Receiver<io::Result<Vec<u8>>>>,
    program: &str,
    stream: &str,
) -> Result<Vec<u8>, String> {
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };

    match reader.recv_timeout(PIPE_FINISH_TIMEOUT) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(format!("failed to read {program} {stream}: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "timed out while draining {program} {stream}; a descendant may still hold the pipe"
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(format!("{program} {stream} reader stopped unexpectedly"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn captured_output_is_bounded() {
        let output = output(
            "sh",
            &["-c", "yes x | head -c 1200000"],
            Duration::from_secs(5),
        )
        .expect("the command should finish successfully");

        assert!(output.contains("[output truncated]"));
        assert!(output.len() <= MAX_CAPTURE_BYTES + OUTPUT_TRUNCATED_MARKER.len());
    }

    #[test]
    fn timeout_terminates_the_process_group() {
        let started = Instant::now();
        let error = output("sh", &["-c", "sleep 30 & wait"], Duration::from_millis(100))
            .expect_err("the command should time out");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn inherited_pipe_from_descendant_does_not_hang_forever() {
        let started = Instant::now();
        let error = output("sh", &["-c", "sleep 30 &"], Duration::from_secs(2))
            .expect_err("the inherited pipe should be detected");

        assert!(error.contains("descendant may still hold the pipe"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
