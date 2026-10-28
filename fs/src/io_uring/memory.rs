use {
    crate::IoSize,
    agave_io_uring::{Ring, RingOp},
    std::{io, slice},
};

// We use fixed buffers to save the cost of mapping/unmapping them at each operation.
//
// Instead of doing many large allocations and registering those, we do a single large one
// and chunk it in slices of up to 1G each.
const FIXED_BUFFER_LEN: IoSize = 1024 * 1024 * 1024;

/// Mutable view into externally allocated bytes buffer for use in io-uring operations.
///
/// The underlying buffer might be registered in `io_uring` for access through "fixed"
/// IO operations, in which case the `registered_io_buf_index` field will be set.
///
/// Stored `ptr` and `size` are used as an unsafe (no lifetime tracking) equivalent of `&mut [u8]`.
#[derive(Debug)]
pub(super) struct IoBufferChunk {
    ptr: *mut u8,
    size: IoSize,
    /// IO buffer index identifying part of the underlying buffer if it was registered in `io_uring`.
    ///
    /// It is used in `ReadFixed` and `WriteFixed` opcodes. The index doesn't identify the chunk
    /// uniquely, since usually chunks are smaller than `FIXED_BUFFER_LEN`.
    registered_io_buf_index: Option<u16>,
}

impl IoBufferChunk {
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            size: 0,
            registered_io_buf_index: None,
        }
    }

    /// Split buffer into `chunk_size` sized [`IoBufferChunk`] buffers for use as registered
    /// buffer in `io_uring` operations.
    #[allow(clippy::arithmetic_side_effects)]
    pub unsafe fn split_buffer_chunks(
        buffer: &mut [u8],
        chunk_size: IoSize,
        registered_buffer: bool,
    ) -> impl Iterator<Item = Self> + use<'_> {
        assert!(
            chunk_size <= FIXED_BUFFER_LEN,
            "chunk size {chunk_size} is too large"
        );
        assert!(
            (buffer.len() / chunk_size as usize) <= u16::MAX as usize,
            "buffer too large (yields too many chunks at size={chunk_size})"
        );
        let buf_start = buffer.as_ptr().addr();
        buffer
            .chunks_exact_mut(chunk_size as usize)
            .map(move |buf| {
                let io_buf_index = (buf.as_ptr().addr() - buf_start) / FIXED_BUFFER_LEN as usize;
                Self {
                    ptr: buf.as_mut_ptr(),
                    size: buf.len() as IoSize,
                    registered_io_buf_index: registered_buffer.then_some(io_buf_index as u16),
                }
            })
    }

    pub fn len(&self) -> IoSize {
        self.size
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Safety: while just returning without dereferencing a pointer is safe, this is marked unsafe
    /// so that the callers are encouraged to reason about the lifetime of the buffer.
    pub unsafe fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The index of the fixed buffer in the ring. See register_buffers().
    pub fn io_buf_index(&self) -> Option<u16> {
        self.registered_io_buf_index
    }

    /// Fill the buffer starting at offset 0 from the `reader`. Return number of bytes read.
    pub fn fill_from_reader(&mut self, mut reader: impl io::Read) -> io::Result<IoSize> {
        // Safety: caller owns the &mut buffer indicating that it is not used by io-uring op
        let mut dst = unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), self.len() as usize) };
        loop {
            let len = reader.read(dst)?;
            if len == dst.len() {
                return Ok(self.len());
            }
            if len == 0 {
                return Ok(self.len().wrapping_sub(dst.len() as IoSize));
            }
            dst = &mut dst[len..];
        }
    }

    /// Copy bytes from `buf` into the buffer at the provided offset.
    pub fn copy_from_slice(&mut self, buf: &[u8], offset: IoSize) {
        let offset = offset as usize;
        let end = offset.strict_add(buf.len());
        assert!(end <= self.len() as usize);
        // Safety: caller owns the &mut buffer indicating that it is not used by io-uring op,
        // `end` is checked above to ensure the buffer is large enough
        let dst = unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), end) };
        dst[offset..].copy_from_slice(buf);
    }

    /// Register provided buffer as fixed buffer in `io_uring`.
    pub unsafe fn register<S, E: RingOp<S>>(
        buffer: &mut [u8],
        ring: &Ring<S, E>,
    ) -> io::Result<()> {
        let iovecs = buffer
            .chunks(FIXED_BUFFER_LEN as usize)
            .map(|buf| libc::iovec {
                iov_base: buf.as_ptr() as _,
                iov_len: buf.len(),
            })
            .collect::<Vec<_>>();
        unsafe { ring.register_buffers(&iovecs) }
    }
}
