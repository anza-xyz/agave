use std::os::{fd::RawFd, unix::net::UnixStream};

/// Orchestrator places the FD at this ID.
pub const ORCHESTRATOR_FD: RawFd = 3;

/// Recovers the orchestrator UDS from the well-known fd ([`ORCHESTRATOR_FD`]).
///
/// # Panics
///
/// Panics if [`ORCHESTRATOR_FD`] is not open or is not a Unix socket.
///
/// # Safety
///
/// The caller must ensure no other code has taken ownership of [`ORCHESTRATOR_FD`]
/// (this includes via FD ID collision if this process was spawned outside of an
/// orchestrator session).
pub unsafe fn orchestrator_uds() -> UnixStream {
    use std::os::fd::FromRawFd;

    // SAFETY:
    // - `mem::zeroed` is valid for `libc::stat`.
    // - `libc::fstat` is safe for any i32.
    // - Caller ensures `ORCHESTRATOR_FD` has not already been claimed.
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        assert!(
            libc::fstat(ORCHESTRATOR_FD, &mut stat) == 0,
            "orchestrator UDS fd 3 is not open (errno={})",
            std::io::Error::last_os_error(),
        );
        assert!(
            (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK,
            "orchestrator UDS fd 3 is not a socket (st_mode={:#o})",
            stat.st_mode,
        );

        UnixStream::from_raw_fd(ORCHESTRATOR_FD)
    }
}
