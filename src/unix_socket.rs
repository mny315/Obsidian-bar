use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::Duration,
};

pub(crate) fn connect(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    // On Linux SO_SNDTIMEO bounds connect when a local server listen queue
    // is full. UnixStream::connect itself has no timeout.
    let path = path.as_os_str().as_bytes();
    // SAFETY: all-zero bytes form a valid sockaddr_un; set its family below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.is_empty() || path.len() >= address.sun_path.len() || path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(path) {
        *target = *byte as libc::c_char;
    }

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned a fresh, owned descriptor of the requested type.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_write_timeout(Some(timeout))?;
    let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
    // SAFETY: address is initialized and address_len includes the trailing NUL
    // within its sun_path buffer. stream owns the descriptor for the whole call.
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            address_len as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connecting_to_a_full_listen_queue_is_bounded() {
        use std::os::unix::net::UnixListener;

        let path =
            std::env::temp_dir().join(format!("obsidian-connect-test-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).expect("bind local test socket");
        // One queued connection is enough to fill a zero-backlog listener.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = connect(&path, Duration::from_millis(40)).unwrap();
        let started = std::time::Instant::now();
        let result = connect(&path, Duration::from_millis(40));
        std::fs::remove_file(&path).unwrap();

        let error = result.expect_err("a full socket queue must time out");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn invalid_socket_paths_are_rejected() {
        for path in ["", "a\0b", &"a".repeat(108)] {
            let error = connect(Path::new(path), Duration::from_secs(1)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
