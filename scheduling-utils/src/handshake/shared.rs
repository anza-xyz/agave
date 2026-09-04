use {
    agave_scheduler_bindings::{
        CheckWorkerToPackMessage, ExecutionWorkerToPackMessage, PackToCheckWorkerMessage,
        PackToExecutionWorkerMessage, ProgressMessage, TpuToPackMessage,
    },
    rts_alloc::Allocator,
    std::fmt,
    thiserror::Error,
};

pub(crate) type RtsAllocError = rts_alloc::error::Error;
pub(crate) type ShaqError = shaq::error::Error;

pub const MAX_WORKERS: usize = 64;

/// Handshake protocol version.
pub(crate) const HANDSHAKE_VERSION: u64 = 6;
pub(crate) const LOGON_SUCCESS: u8 = 0x01;
pub(crate) const LOGON_FAILURE: u8 = 0x02;
pub(crate) const MAX_ALLOCATOR_HANDLES: usize = 128;
pub(crate) const GLOBAL_ALLOCATORS: usize = 1;

/// Versions of the interfaces shared by Agave and an external scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ProtocolVersions {
    /// Version of the handshake protocol.
    pub handshake: u64,
    /// Version of the scheduler bindings.
    pub scheduler_bindings: u64,
    /// Version of the shared-memory queues.
    pub shaq: u64,
    /// Version of the shared-memory allocator.
    pub rts_alloc: u64,
}

impl ProtocolVersions {
    pub(crate) const SERIALIZED_SIZE: usize = core::mem::size_of::<Self>();

    /// Returns the versions used by this build.
    pub fn current() -> Self {
        Self {
            handshake: HANDSHAKE_VERSION,
            scheduler_bindings: agave_scheduler_bindings::version(),
            shaq: u64::from(shaq::VERSION),
            rts_alloc: u64::from(rts_alloc::VERSION),
        }
    }

    pub(crate) fn try_from_bytes(buffer: &[u8]) -> Option<Self> {
        if buffer.len() != Self::SERIALIZED_SIZE {
            return None;
        }

        // SAFETY:
        // - buffer is correctly sized, initialized and readable.
        // - `Self` is valid for any byte pattern.
        Some(unsafe { core::ptr::read_unaligned(buffer.as_ptr().cast()) })
    }
}

impl fmt::Display for ProtocolVersions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "handshake={}, scheduler-bindings={}, shaq={}, rts-alloc={}",
            self.handshake, self.scheduler_bindings, self.shaq, self.rts_alloc
        )
    }
}

/// The logon message sent by the client to the server.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct ClientLogon {
    /// The number of Agave worker threads that will be spawned to handle packing requests.
    pub worker_count: usize,
    /// The number of Agave check worker threads that will be spawned to handle check requests.
    pub check_worker_count: usize,
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
    /// The minimum capacity of the scheduler-to-check-worker queue in messages.
    pub pack_to_check_worker_capacity: usize,
    /// The minimum capacity of the check-worker-to-scheduler queue in messages.
    pub check_worker_to_pack_capacity: usize,
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

pub mod logon_flags {}

/// The complete initialized scheduling session.
pub struct ClientSession {
    pub allocators: Vec<Allocator>,
    pub tpu_to_pack: shaq::spsc::Consumer<TpuToPackMessage>,
    pub progress_tracker: shaq::spsc::Consumer<ProgressMessage>,
    pub pack_to_check_worker: shaq::mpmc::Producer<PackToCheckWorkerMessage>,
    pub check_worker_to_pack: shaq::mpmc::Consumer<CheckWorkerToPackMessage>,
    pub workers: Vec<ClientWorkerSession>,
}

/// A per worker scheduling session.
pub struct ClientWorkerSession {
    pub pack_to_worker: shaq::spsc::Producer<PackToExecutionWorkerMessage>,
    pub worker_to_pack: shaq::spsc::Consumer<ExecutionWorkerToPackMessage>,
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
    pub check_workers: Vec<AgaveCheckWorkerSession>,
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
    pub pack_to_worker: shaq::spsc::Consumer<PackToExecutionWorkerMessage>,
    pub worker_to_pack: shaq::spsc::Producer<ExecutionWorkerToPackMessage>,
}

/// Shared memory objects for a single check worker.
pub struct AgaveCheckWorkerSession {
    pub allocator: Allocator,
    pub pack_to_check_worker: shaq::mpmc::Consumer<PackToCheckWorkerMessage>,
    pub check_worker_to_pack: shaq::mpmc::Producer<CheckWorkerToPackMessage>,
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
    #[error("Version; server=({server}); client=({client})")]
    Version {
        server: ProtocolVersions,
        client: ProtocolVersions,
    },
    #[error("Worker count; count={0}")]
    WorkerCount(usize),
    #[error("Check worker count; count={0}")]
    CheckWorkerCount(usize),
    #[error("Allocator handles; count={0}")]
    AllocatorHandles(usize),
    #[error("Rts alloc; err={0:?}")]
    RtsAlloc(#[from] RtsAllocError),
    #[error("Shaq; err={0:?}")]
    Shaq(#[from] ShaqError),
}
