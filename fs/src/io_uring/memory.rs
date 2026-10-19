use {
    crate::IoSize,
    agave_io_uring::{Ring, RingOp},
    std::{
        alloc::{Layout, LayoutError, alloc},
        io,
        ops::{Deref, DerefMut},
        ptr::{self, NonNull},
        slice,
    },
};

// We use fixed buffers to save the cost of mapping/unmapping them at each operation.
//
// Instead of doing many large allocations and registering those, we do a single large one
// and chunk it in slices of up to 1G each.
const FIXED_BUFFER_LEN: IoSize = 1024 * 1024 * 1024;

#[derive(thiserror::Error, Debug)]
pub enum AllocError {
    #[error("out of memory")]
    OutOfMemory,

    #[error("invalid layout parameters: {0}")]
    LayoutCreation(#[from] LayoutError),

    #[error("can't allocate a zero size buffer")]
    LayoutSize,

    #[error("libc mmap failed")]
    MMapFailed,
}

impl From<AllocError> for io::Error {
    fn from(error: AllocError) -> io::Error {
        match error {
            AllocError::OutOfMemory => io::Error::new(io::ErrorKind::OutOfMemory, "out of memory"),
            AllocError::LayoutCreation(layout_err) => {
                io::Error::new(io::ErrorKind::InvalidInput, layout_err)
            }
            AllocError::LayoutSize => io::Error::new(
                io::ErrorKind::InvalidInput,
                "can't allocate a zero size buffer",
            ),
            _ => io::Error::other(error),
        }
    }
}

enum AllocationMethod {
    Std(Layout),
    Mmap,
}

/// Size and alignment of a transparent huge page with 4 KiB base pages, as on x86_64.
const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

/// Memory buffer whose base address is aligned to the system page size.
///
/// `ptr` is always aligned to the system page size.
/// `len` is not guaranteed to be page-aligned.
///
/// Buffers of at least [`HUGE_PAGE_SIZE`] start at a huge page boundary and ask the kernel to
/// back them with transparent huge pages (granted when THP is enabled in `always` or `madvise`
/// mode). Huge pages cut the TLB misses of sweeps through large buffers, like copying data out of
/// them after reads.
pub struct PageAlignedMemory {
    ptr: NonNull<u8>,
    len: usize,
    allocation_method: AllocationMethod,
}

impl PageAlignedMemory {
    /// Allocate memory buffer optimized for io_uring operations, i.e.
    /// using huge pages when they are available on the host.
    pub fn new(size: usize) -> Result<Self, AllocError> {
        if size >= HUGE_PAGE_SIZE {
            return Self::new_huge_page_aligned(size);
        }

        let layout = Layout::from_size_align(size, Self::page_size())?;
        if layout.size() == 0 {
            return Err(AllocError::LayoutSize);
        }
        // Safety:
        // layout size is nonzero
        let ptr = unsafe { alloc(layout) };

        Ok(Self {
            ptr: NonNull::new(ptr).ok_or(AllocError::OutOfMemory)?,
            len: size,
            allocation_method: AllocationMethod::Std(layout),
        })
    }

    fn new_huge_page_aligned(size: usize) -> Result<Self, AllocError> {
        let aligned_size = size.next_multiple_of(Self::page_size());
        // Over-map by one huge page, so the buffer can start at a huge page boundary.
        let map_len = aligned_size.strict_add(HUGE_PAGE_SIZE);
        // Safety:
        // doing an ANONYMOUS alloc. addr=NULL is ok, fd is not used.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        if std::ptr::eq(ptr, libc::MAP_FAILED) {
            return Err(AllocError::MMapFailed);
        }

        let map_start = ptr as usize;
        let map_end = map_start.strict_add(map_len);
        let start = map_start.next_multiple_of(HUGE_PAGE_SIZE);
        let end = start.strict_add(aligned_size);
        // Safety:
        // the trimmed head and tail lie inside the mapping above and nothing references them.
        // The advice only affects how the kernel backs the range, so its failure (e.g. a kernel
        // without THP) leaves a valid buffer of base pages.
        unsafe {
            if start > map_start {
                libc::munmap(ptr, start.strict_sub(map_start));
            }
            if map_end > end {
                libc::munmap(end as *mut libc::c_void, map_end.strict_sub(end));
            }
            libc::madvise(
                start as *mut libc::c_void,
                aligned_size,
                libc::MADV_HUGEPAGE,
            );
        }

        Ok(Self {
            ptr: NonNull::new(start as *mut u8).ok_or(AllocError::OutOfMemory)?,
            len: size,
            allocation_method: AllocationMethod::Mmap,
        })
    }

    fn page_size() -> usize {
        // Safety: just a libc wrapper
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }
}

impl Drop for PageAlignedMemory {
    fn drop(&mut self) {
        match self.allocation_method {
            AllocationMethod::Std(layout) => {
                // Safety:
                // ptr was allocated with the same layout
                unsafe {
                    std::alloc::dealloc(self.ptr.as_ptr(), layout);
                }
            }
            AllocationMethod::Mmap => {
                // Safety:
                // ptr starts the mapping kept by `new_huge_page_aligned`, and munmap rounds len up
                // to the page size it was mapped with
                unsafe {
                    libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len);
                }
            }
        }
    }
}

impl Deref for PageAlignedMemory {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for PageAlignedMemory {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_buffer_is_page_aligned() {
        let mut buf = PageAlignedMemory::new(HUGE_PAGE_SIZE - 1).unwrap();
        assert!(matches!(buf.allocation_method, AllocationMethod::Std(_)));
        assert!(
            buf.as_ptr()
                .addr()
                .is_multiple_of(PageAlignedMemory::page_size())
        );
        assert_eq!(buf.len(), HUGE_PAGE_SIZE - 1);
        buf.fill(1);
    }

    #[test]
    fn test_large_buffer_starts_at_huge_page() {
        for size in [HUGE_PAGE_SIZE, 3 * HUGE_PAGE_SIZE + 4097] {
            let mut buf = PageAlignedMemory::new(size).unwrap();
            assert!(matches!(buf.allocation_method, AllocationMethod::Mmap));
            assert!(buf.as_ptr().addr().is_multiple_of(HUGE_PAGE_SIZE));
            assert_eq!(buf.len(), size);
            buf.fill(1);
            assert!(buf.iter().all(|&b| b == 1));
        }
    }
}
