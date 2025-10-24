pub(crate) const VERSION: u64 = 1;
pub(crate) const LOGON_SUCCESS: u8 = 0x01;
pub(crate) const LOGON_FAILURE: u8 = 0x02;
pub(crate) const MAX_WORKERS: usize = 64;
pub(crate) const MAX_ALLOCATOR_HANDLES: usize = 128;
pub(crate) const GLOBAL_ALLOCATORS: usize = 1;

/// The logon message sent by the client to the server.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct ClientLogon {
    /// The number of Agave worker threads that will be spawned to handle packing requests.
    pub worker_count: usize,
    /// The allocator file size in bytes, this is shared by all allocator handles.
    pub allocator_size: usize,
    /// The number of [`rts_alloc::Allocator`] handles the external process is requesting.
    pub allocator_handles: usize,
    /// The size of the `tpu_to_pack` queue in bytes.
    pub tpu_to_pack_size: usize,
    /// The size of the `progress_tracker` queue in bytes.
    pub progress_tracker_size: usize,
    /// The size of the `pack_to_worker` queue in bytes.
    pub pack_to_worker_size: usize,
    /// The size of the `worker_to_pack` queue in bytes.
    pub worker_to_pack_size: usize,
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
