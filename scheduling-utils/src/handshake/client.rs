use {
    super::shared::MAX_WORKERS,
    crate::handshake::{
        ClientHandshakeError, ClientLogon, ClientSession,
        shared::{GLOBAL_SHMEM, HANDSHAKE_BUFFER_SIZE, LOGON_FAILURE, join_session, send_logon},
    },
    libc::CMSG_LEN,
    nix::sys::socket::{self, ControlMessageOwned, MsgFlags, UnixAddr},
    std::{
        fs::File,
        io::IoSliceMut,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::net::UnixStream,
        },
        path::Path,
        time::Duration,
    },
};

/// The maximum size in bytes of the control message containing the queues assuming [`MAX_WORKERS`]
/// is respected.
///
/// Each FD is 4 bytes so we simply multiply the number of shmem objects by 4 to get the control
/// message buffer size.
const CMSG_MAX_SIZE: usize = (GLOBAL_SHMEM + MAX_WORKERS * 2) * 4;

/// Connects to the scheduler server on the given IPC path.
///
/// # Timeout
///
/// Timeout is enforced at the syscall level. In the typical case, this function will do two
/// syscalls, one to send the logon message and one to receive the response. However, if for
/// whatever reason the OS does not accept 1024 bytes in a single syscall, then multiple writes
/// could be needed. As such this timeout is meant to guard against a broken server but not
/// necessarily ensure this function always returns before the timeout (this is somewhat in line
/// with typical timeouts because you have no guarantee of being rescheduled).
pub fn connect(
    path: impl AsRef<Path>,
    logon: ClientLogon,
    timeout: Duration,
) -> Result<ClientSession, ClientHandshakeError> {
    connect_path(path.as_ref(), logon, timeout)
}

fn connect_path(
    path: &Path,
    logon: ClientLogon,
    timeout: Duration,
) -> Result<ClientSession, ClientHandshakeError> {
    // NB: Technically this connect call can block indefinitely if the receiver's connection queue
    // is full. In practice this should almost never happen. If it does work arounds are:
    //
    // - Users can spawn off a thread to handle the connect call and then just poll that thread
    //   exiting.
    // - This library could drop to raw unix sockets and use select/poll to enforce a timeout on the
    //   IO operation.
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Send the logon message to the server.
    send_logon(&mut stream, logon)?;

    // Receive the server's response & on success the files for the newly allocated shared memory.
    let files = recv_response(&mut stream)?;

    // Join the shared memory regions.
    let session = join_session(&logon, files)?;

    Ok(session)
}

fn recv_response(stream: &mut UnixStream) -> Result<Vec<File>, ClientHandshakeError> {
    // Receive the requested FDs.
    let mut buf = [0; HANDSHAKE_BUFFER_SIZE];
    let mut iov = [IoSliceMut::new(&mut buf)];
    // SAFETY: CMSG_LEN is always safe (const expression).
    let mut cmsgs = [0u8; unsafe { CMSG_LEN(CMSG_MAX_SIZE as u32) as usize }];
    let msg = socket::recvmsg::<UnixAddr>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsgs),
        MsgFlags::empty(),
    )?;

    // Check for failure.
    let buf = msg.iovs().next().unwrap();
    if buf[0] == LOGON_FAILURE {
        let reason_len = usize::from(buf[1]);
        #[allow(clippy::arithmetic_side_effects)]
        let reason = std::str::from_utf8(&buf[2..2 + reason_len]).unwrap();

        return Err(ClientHandshakeError::Rejected(reason.to_string()));
    }

    // Extract FDs and immediately wrap in `File` for RAII ownership.
    let mut cmsgs = msg.cmsgs().unwrap();
    let fds = match cmsgs.next() {
        Some(ControlMessageOwned::ScmRights(fds)) => fds,
        Some(msg) => panic!("Unexpected; msg={msg:?}"),
        None => panic!(),
    };
    // SAFETY: FDs were just received via `ScmRights` and are valid.
    let files = fds
        .into_iter()
        .map(|fd| unsafe { File::from_raw_fd(fd) })
        .collect();

    Ok(files)
}

pub fn setup_session(
    logon: &ClientLogon,
    files: Vec<File>,
) -> Result<ClientSession, ClientHandshakeError> {
    join_session(logon, files)
}

impl From<nix::Error> for ClientHandshakeError {
    fn from(value: nix::Error) -> Self {
        Self::Io(value.into())
    }
}
