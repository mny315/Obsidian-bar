//! Wait for a child and drain both pipes on the calling backend worker.
use super::{
    MAX_CAPTURE_BYTES, OUTPUT_TRUNCATED_MARKER, PIPE_FINISH_TIMEOUT, PROCESS_TERMINATION_GRACE,
};
use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::{Child, ExitStatus, Output},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(super) enum Error {
    Io(io::Error),
    TimedOut,
    PipeTimeout(&'static str),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Default)]
struct Pipe {
    file: Option<File>,
    bytes: Vec<u8>,
    truncated: bool,
}

impl Pipe {
    fn open(fd: Option<OwnedFd>) -> io::Result<Self> {
        let file = fd.map(File::from);
        if let Some(file) = &file {
            // SAFETY: fcntl only reads and updates flags of this owned pipe.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            file,
            ..Self::default()
        })
    }

    fn drain(&mut self) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let mut buffer = [0_u8; 8192];
        // A continuously writing child must not prevent deadline checks or
        // starve the other pipe. Remaining bytes stay readable for the next poll.
        for _ in 0..16 {
            match file.read(&mut buffer) {
                Ok(0) => {
                    self.file.take();
                    break;
                }
                Ok(read) => {
                    let kept = read.min(MAX_CAPTURE_BYTES.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..kept]);
                    self.truncated |= kept < read;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        if self.truncated {
            self.bytes.extend_from_slice(OUTPUT_TRUNCATED_MARKER);
        }
        self.bytes
    }
}

struct Process {
    child: Option<Child>,
    group: u32,
    pidfd: Option<OwnedFd>,
    stdout: Pipe,
    stderr: Pipe,
}

impl Process {
    fn new(mut child: Child) -> io::Result<Self> {
        let group = child.id();
        let stdout = child.stdout.take().map(OwnedFd::from);
        let stderr = child.stderr.take().map(OwnedFd::from);
        let mut process = Self {
            child: Some(child),
            group,
            pidfd: pidfd(group),
            stdout: Pipe::default(),
            stderr: Pipe::default(),
        };
        process.stdout = Pipe::open(stdout)?;
        process.stderr = Pipe::open(stderr)?;
        Ok(process)
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        loop {
            self.stdout.drain()?;
            self.stderr.drain()?;
            if let Some(status) = self.child.as_mut().expect("unreaped child").try_wait()? {
                self.child.take();
                self.pidfd.take();
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            self.poll(deadline)?;
        }
    }

    fn poll(&self, deadline: Instant) -> io::Result<()> {
        let mut fds = [libc::pollfd {
            fd: -1,
            events: libc::POLLIN,
            revents: 0,
        }; 3];
        for (poll, fd) in fds.iter_mut().zip([
            self.pidfd.as_ref().map(AsRawFd::as_raw_fd),
            self.stdout.file.as_ref().map(AsRawFd::as_raw_fd),
            self.stderr.file.as_ref().map(AsRawFd::as_raw_fd),
        ]) {
            poll.fd = fd.unwrap_or(-1);
        }
        let mut remaining = deadline.saturating_duration_since(Instant::now());
        if self.child.is_some() && self.pidfd.is_none() {
            // Older kernels and sandbox policies can disable pidfds. Retain
            // bounded waits without creating a waiter thread for each command.
            remaining = remaining.min(Duration::from_millis(20));
        }
        let timeout = remaining.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // SAFETY: fds is initialized, and all nonnegative descriptors remain
        // owned by self for the duration of poll.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }

    fn terminate(&mut self) {
        signal_group(self.group, libc::SIGTERM);
        if self.child.is_some() {
            let _ = self.wait_until(Instant::now() + PROCESS_TERMINATION_GRACE);
        }
        // Also kill descendants when the leader already exited after TERM.
        signal_group(self.group, libc::SIGKILL);
        if self.child.is_some() {
            let _ = self.wait_until(Instant::now() + PIPE_FINISH_TIMEOUT);
        }
    }

    fn finish_pipes(&mut self) -> Result<(), Error> {
        let deadline = Instant::now() + PIPE_FINISH_TIMEOUT;
        loop {
            self.stdout.drain()?;
            self.stderr.drain()?;
            if self.stdout.file.is_none() && self.stderr.file.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::PipeTimeout(if self.stdout.file.is_some() {
                    "stdout"
                } else {
                    "stderr"
                }));
            }
            self.poll(deadline)?;
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            signal_group(self.group, libc::SIGKILL);
            // Normally SIGKILL has already been reaped above. A process stuck
            // in kernel I/O must not hold the backend worker indefinitely.
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = std::thread::Builder::new()
                    .name("process-reaper".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            }
        }
    }
}

pub(super) fn wait(child: Child, timeout: Duration) -> Result<Output, Error> {
    let deadline = Instant::now() + timeout;
    let mut process = Process::new(child)?;
    let Some(status) = process.wait_until(deadline)? else {
        process.terminate();
        return Err(Error::TimedOut);
    };
    if let Err(error) = process.finish_pipes() {
        signal_group(process.group, libc::SIGKILL);
        return Err(error);
    }
    Ok(Output {
        status,
        stdout: std::mem::take(&mut process.stdout).finish(),
        stderr: std::mem::take(&mut process.stderr).finish(),
    })
}

fn pidfd(pid: u32) -> Option<OwnedFd> {
    // SAFETY: pidfd_open has no pointer arguments and returns a fresh owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn signal_group(group: u32, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(group)
        && pid > 0
    {
        // SAFETY: a negative PID signals this child's separately created group.
        unsafe { libc::kill(-pid, signal) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::process::CommandExt, process::Command};

    #[test]
    fn child_wait_also_works_when_pidfds_are_unavailable() {
        let child = Command::new("sh")
            .args(["-c", "sleep 0.02"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut process = Process::new(child).unwrap();
        process.pidfd.take();
        let status = process
            .wait_until(Instant::now() + Duration::from_secs(2))
            .unwrap()
            .expect("fallback must observe the child exit");
        assert!(status.success());
        process.finish_pipes().unwrap();
    }
}
