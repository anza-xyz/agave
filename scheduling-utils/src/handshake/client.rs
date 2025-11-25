use {
    crate::handshake::{
        shared::{GLOBAL_ALLOCATORS, LOGON_FAILURE, MAX_WORKERS, VERSION},
        ClientLogon,
    },
    agave_scheduler_bindings::{
        PackToWorkerMessage, ProgressMessage, TpuToPackMessage, WorkerToPackMessage,
    },
    libc::CMSG_LEN,
    nix::sys::socket::{self, ControlMessageOwned, MsgFlags, UnixAddr},
    rts_alloc::Allocator,
    std::{
        fs::File,
        io::{IoSliceMut, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::net::UnixStream,
        },
        path::Path,
        time::Duration,
    },
    thiserror::Error,
};

type RtsError = rts_alloc::error::Error;
type ShaqError = shaq::error::Error;

/// Number of global shared memory objects (in addition to per worker objects).
const GLOBAL_SHMEM: usize = 3;

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

    // Receive the server's response & on success the FDs for the newly allocated shared memory.
    let fds = recv_response(&mut stream)?;

    // Join the shared memory regions.
    let session = setup_session(&logon, fds)?;

    Ok(session)
}

fn send_logon(stream: &mut UnixStream, logon: ClientLogon) -> Result<(), ClientHandshakeError> {
    // Send the logon message.
    let mut buf = [0; 1024];
    buf[..8].copy_from_slice(&VERSION.to_le_bytes());
    const LOGON_END: usize = 8 + core::mem::size_of::<ClientLogon>();
    let ptr = buf[8..LOGON_END].as_mut_ptr().cast::<ClientLogon>();
    // SAFETY:
    // - `buf` is valid for writes.
    // - `buf.len()` has enough space for logon's size in memory.
    unsafe {
        core::ptr::write_unaligned(ptr, logon);
    }
    stream.write_all(&buf)?;

    Ok(())
}

fn recv_response(stream: &mut UnixStream) -> Result<Vec<i32>, ClientHandshakeError> {
    // Receive the requested FDs.
    let mut buf = [0; 1024];
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

    // Extract FDs.
    let mut cmsgs = msg.cmsgs().unwrap();
    let fds = match cmsgs.next() {
        Some(ControlMessageOwned::ScmRights(fds)) => fds,
        Some(msg) => panic!("Unexpected; msg={msg:?}"),
        None => panic!(),
    };

    Ok(fds)
}

pub fn setup_session(
    logon: &ClientLogon,
    fds: Vec<i32>,
) -> Result<ClientSession, ClientHandshakeError> {
    let [allocator_fd, tpu_to_pack_fd, progress_tracker_fd] = fds[..GLOBAL_SHMEM] else {
        panic!();
    };
    // SAFETY: `allocator_fd` represents a valid file descriptor that was just returned to us via
    // `ScmRights`.
    let allocator_file = unsafe { File::from_raw_fd(allocator_fd) };
    let worker_fds = &fds[GLOBAL_SHMEM..];

    // Setup requested allocators.
    let allocators = (0..logon.allocator_handles)
        .map(|offset| {
            // NB: Server validates all requested counts are within expected bands so this should
            // never panic.
            let id = GLOBAL_ALLOCATORS
                .checked_add(logon.worker_count)
                .unwrap()
                .checked_add(offset)
                .unwrap();

            unsafe { Allocator::join(&allocator_file, u32::try_from(id).unwrap()) }
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Ensure worker_fds length matches expectations.
    if worker_fds.is_empty()
        || !worker_fds.len().is_multiple_of(2)
        || worker_fds.len() / 2 != logon.worker_count
    {
        return Err(ClientHandshakeError::ProtocolViolation);
    }

    // NB: After creating & mapping the queues we are fine to drop the FDs as mmap will keep the
    // underlying object alive until process exit or munmap.
    let session = ClientSession {
        allocators,
        tpu_to_pack: unsafe { shaq::Consumer::join(&File::from_raw_fd(tpu_to_pack_fd))? },
        progress_tracker: unsafe { shaq::Consumer::join(&File::from_raw_fd(progress_tracker_fd))? },
        workers: worker_fds
            .chunks(2)
            .map(|window| {
                let [pack_to_worker, worker_to_pack] = window else {
                    panic!();
                };

                Ok(ClientWorkerSession {
                    pack_to_worker: unsafe {
                        shaq::Producer::join(&File::from_raw_fd(*pack_to_worker))?
                    },
                    worker_to_pack: unsafe {
                        shaq::Consumer::join(&File::from_raw_fd(*worker_to_pack))?
                    },
                })
            })
            .collect::<Result<_, ClientHandshakeError>>()?,
    };

    Ok(session)
}

/// The complete initialized scheduling session.
pub struct ClientSession {
    pub allocators: Vec<Allocator>,
    pub tpu_to_pack: shaq::Consumer<TpuToPackMessage>,
    pub progress_tracker: shaq::Consumer<ProgressMessage>,
    pub workers: Vec<ClientWorkerSession>,
}

/// An per worker scheduling session.
pub struct ClientWorkerSession {
    pub pack_to_worker: shaq::Producer<PackToWorkerMessage>,
    pub worker_to_pack: shaq::Consumer<WorkerToPackMessage>,
}

/// Potential errors that can occur during the client's side of the handshake.
#[derive(Debug, Error)]
pub enum ClientHandshakeError {
    #[error("Io; err={0}")]
    Io(#[from] std::io::Error),
    #[error("Timed out")]
    TimedOut,
    #[error("Protocol violation")]
    ProtocolViolation,
    #[error("Rejected; reason={0}")]
    Rejected(String),
    #[error("Rts alloc; err={0}")]
    RtsAlloc(#[from] RtsError),
    #[error("Shaq; err={0}")]
    Shaq(#[from] ShaqError),
}

impl From<nix::Error> for ClientHandshakeError {
    fn from(value: nix::Error) -> Self {
        Self::Io(value.into())
    }
}
