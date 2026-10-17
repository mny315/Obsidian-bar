use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    sync::OnceLock,
    time::Duration,
};

mod capture;

const PROCESS_TERMINATION_GRACE: Duration = Duration::from_millis(250);
const PIPE_FINISH_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const OUTPUT_TRUNCATED_MARKER: &[u8] = b"\n[output truncated]\n";

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

    match capture::wait(child, timeout) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(StatusError::Failed),
        Err(capture::Error::TimedOut) => Err(StatusError::TimedOut),
        Err(capture::Error::Io(error)) => Err(StatusError::Io(error)),
        Err(capture::Error::PipeTimeout(stream)) => Err(StatusError::Io(io::Error::other(
            format!("timed out while draining {stream}"),
        ))),
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
    let child = command
        .spawn()
        .map_err(|error| format!("failed to start {display_name}: {error}"))?;

    let output = capture::wait(child, timeout).map_err(|error| match error {
        capture::Error::TimedOut => {
            format!("{display_name} timed out after {} ms", timeout.as_millis())
        }
        capture::Error::Io(error) => format!("failed to wait for {display_name}: {error}"),
        capture::Error::PipeTimeout(stream) => format!(
            "timed out while draining {display_name} {stream}; a descendant may still hold the pipe"
        ),
    })?;
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
    fn stdout_and_stderr_are_drained_together_without_exceeding_the_limits() {
        let output = run(
            "sh",
            &[
                "-c",
                "head -c 1500000 /dev/zero & head -c 1500000 /dev/zero >&2; wait",
            ],
            Duration::from_secs(5),
            true,
        )
        .expect("neither full pipe should block the child");
        for bytes in [&output.stdout, &output.stderr] {
            assert_eq!(
                bytes.len(),
                MAX_CAPTURE_BYTES + OUTPUT_TRUNCATED_MARKER.len()
            );
            assert!(bytes.ends_with(OUTPUT_TRUNCATED_MARKER));
        }
    }

    #[test]
    fn continuous_output_cannot_starve_the_timeout() {
        let started = Instant::now();
        let error = output("sh", &["-c", "exec yes x"], Duration::from_millis(100))
            .expect_err("an endless writer must time out");
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
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
    fn timeout_kills_and_reaps_a_process_that_ignores_term() {
        let child = Command::new("sh")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .process_group(0)
            .spawn()
            .expect("spawn child");
        let pid = child.id() as libc::pid_t;
        let started = Instant::now();

        assert!(matches!(
            capture::wait(child, Duration::from_millis(100)),
            Err(capture::Error::TimedOut)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
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
