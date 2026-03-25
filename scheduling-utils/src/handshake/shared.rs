use {
    agave_scheduler_bindings::{
        PackToWorkerMessage, ProgressMessage, TpuToPackMessage, WorkerToPackMessage,
    },
    rts_alloc::Allocator,
    std::{
        fs::File,
        io::{Read, Write},
    },
    thiserror::Error,
};

pub(crate) type RtsAllocError = rts_alloc::error::Error;
pub(crate) type ShaqError = shaq::error::Error;

pub const MAX_WORKERS: usize = 64;

/// Protocol version.
pub(crate) const VERSION: u64 = 3;
pub(crate) const LOGON_SUCCESS: u8 = 0x01;
pub(crate) const LOGON_FAILURE: u8 = 0x02;
pub(crate) const MAX_ALLOCATOR_HANDLES: usize = 128;
pub(crate) const GLOBAL_ALLOCATORS: usize = 1;
pub(crate) const GLOBAL_SHMEM: usize = 3;
pub(crate) const HANDSHAKE_BUFFER_SIZE: usize = 1024;

/// The logon message sent by the client to the server.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct ClientLogon {
    /// The number of Agave worker threads that will be spawned to handle packing requests.
    pub worker_count: usize,
    /// The minimum allocator file size in bytes, this is shared by all allocator handles.
    pub allocator_size: usize,
    /// The number of [`rts_alloc::Allocator`] handles the external process is requesting.
    pub allocator_handles: usize,
    /// The minimum capacity of the `tpu_to_pack` queue in messages.
    pub tpu_to_pack_capacity: usize,
    /// The minimum capacity of the `progress_tracker` queue in messages.
    pub progress_tracker_capacity: usize,
    /// The minimum capacity of the `pack_to_worker` queue in messages.
    pub pack_to_worker_capacity: usize,
    /// The minimum capacity of the `worker_to_pack` queue in messages.
    pub worker_to_pack_capacity: usize,
    /// Flags that control the behavior of the new scheduling session.
    pub flags: u16,
    // NB: If adding more fields please ensure:
    // - The fields are zeroable.
    // - If possible the fields are backwards compatible:
    //   - Added to the end of the struct.
    //   - 0 bytes is valid default (older clients will not have the field and thus send zeroes).
    // - If not backwards compatible, increment the version counter.
}

impl ClientLogon {
    pub fn try_from_bytes(buffer: &[u8]) -> Option<Self> {
        if buffer.len() != core::mem::size_of::<Self>() {
            return None;
        }

        // SAFETY:
        // - buffer is correctly sized, initialized and readable.
        // - `Self` is valid for any byte pattern
        Some(unsafe { core::ptr::read_unaligned(buffer.as_ptr().cast()) })
    }
}

pub(crate) fn send_logon<W: Write>(
    stream: &mut W,
    logon: ClientLogon,
) -> Result<(), ClientHandshakeError> {
    let mut buf = [0; HANDSHAKE_BUFFER_SIZE];
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

pub(crate) fn recv_logon<R: Read>(
    stream: &mut R,
    buffer: &mut [u8; HANDSHAKE_BUFFER_SIZE],
) -> Result<ClientLogon, AgaveHandshakeError> {
    stream.read_exact(buffer)?;

    let version = u64::from_le_bytes(buffer[..8].try_into().unwrap());
    if version != VERSION {
        return Err(AgaveHandshakeError::Version {
            server: VERSION,
            client: version,
        });
    }

    const LOGON_END: usize = 8 + core::mem::size_of::<ClientLogon>();
    let logon = ClientLogon::try_from_bytes(&buffer[8..LOGON_END]).unwrap();

    if !(1..=MAX_WORKERS).contains(&logon.worker_count) {
        return Err(AgaveHandshakeError::WorkerCount(logon.worker_count));
    }

    if !(1..=MAX_ALLOCATOR_HANDLES).contains(&logon.allocator_handles) {
        return Err(AgaveHandshakeError::AllocatorHandles(
            logon.allocator_handles,
        ));
    }

    Ok(logon)
}

pub(crate) fn join_session(
    logon: &ClientLogon,
    files: Vec<File>,
) -> Result<ClientSession, ClientHandshakeError> {
    if files.len() < GLOBAL_SHMEM {
        return Err(ClientHandshakeError::ProtocolViolation);
    }
    let (global_files, worker_files) = files.split_at(GLOBAL_SHMEM);
    let [allocator_file, tpu_to_pack_file, progress_tracker_file] = global_files else {
        unreachable!();
    };

    let allocators = (0..logon.allocator_handles)
        .map(|_| Allocator::join(allocator_file))
        .collect::<Result<Vec<_>, _>>()?;

    if worker_files.is_empty()
        || !worker_files.len().is_multiple_of(2)
        || worker_files.len() / 2 != logon.worker_count
    {
        return Err(ClientHandshakeError::ProtocolViolation);
    }

    let session = ClientSession {
        allocators,
        tpu_to_pack: unsafe { shaq::spsc::Consumer::join(tpu_to_pack_file)? },
        progress_tracker: unsafe { shaq::spsc::Consumer::join(progress_tracker_file)? },
        workers: worker_files
            .chunks(2)
            .map(|window| {
                let [pack_to_worker, worker_to_pack] = window else {
                    panic!();
                };

                Ok(ClientWorkerSession {
                    pack_to_worker: unsafe { shaq::spsc::Producer::join(pack_to_worker)? },
                    worker_to_pack: unsafe { shaq::spsc::Consumer::join(worker_to_pack)? },
                })
            })
            .collect::<Result<_, ClientHandshakeError>>()?,
    };

    drop(files);

    Ok(session)
}

pub mod logon_flags {}

/// The complete initialized scheduling session.
pub struct ClientSession {
    pub allocators: Vec<Allocator>,
    pub tpu_to_pack: shaq::spsc::Consumer<TpuToPackMessage>,
    pub progress_tracker: shaq::spsc::Consumer<ProgressMessage>,
    pub workers: Vec<ClientWorkerSession>,
}

/// A per worker scheduling session.
pub struct ClientWorkerSession {
    pub pack_to_worker: shaq::spsc::Producer<PackToWorkerMessage>,
    pub worker_to_pack: shaq::spsc::Consumer<WorkerToPackMessage>,
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
    RtsAlloc(#[from] RtsAllocError),
    #[error("Shaq; err={0}")]
    Shaq(#[from] ShaqError),
}

/// An initialized scheduling session.
pub struct AgaveSession {
    pub flags: u16,
    pub tpu_to_pack: AgaveTpuToPackSession,
    pub progress_tracker: shaq::spsc::Producer<ProgressMessage>,
    pub workers: Vec<AgaveWorkerSession>,
}

/// Shared memory objects for the tpu to pack worker.
pub struct AgaveTpuToPackSession {
    pub allocator: Allocator,
    pub producer: shaq::spsc::Producer<TpuToPackMessage>,
}

/// Shared memory objects for a single banking worker.
pub struct AgaveWorkerSession {
    pub allocator: Allocator,
    pub pack_to_worker: shaq::spsc::Consumer<PackToWorkerMessage>,
    pub worker_to_pack: shaq::spsc::Producer<WorkerToPackMessage>,
}

/// Potential errors that can occur during the Agave side of the handshake.
///
/// # Note
///
/// These errors are stringified (up to 256 bytes then truncated) and sent to the client.
#[derive(Debug, Error)]
pub enum AgaveHandshakeError {
    #[error("Io; err={0}")]
    Io(#[from] std::io::Error),
    #[error("Timeout")]
    Timeout,
    #[error("Close during handshake")]
    EofDuringHandshake,
    #[error("Version; server={server}; client={client}")]
    Version { server: u64, client: u64 },
    #[error("Worker count; count={0}")]
    WorkerCount(usize),
    #[error("Allocator handles; count={0}")]
    AllocatorHandles(usize),
    #[error("Rts alloc; err={0:?}")]
    RtsAlloc(#[from] RtsAllocError),
    #[error("Shaq; err={0:?}")]
    Shaq(#[from] ShaqError),
}
