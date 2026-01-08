use {
    agave_io_uring::{Ring, RingOp},
    std::{
        alloc::{alloc, Layout, LayoutError},
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
const FIXED_BUFFER_LEN: usize = 1024 * 1024 * 1024;

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

/// Memory buffer whose base address is aligned to the system page size.
///
/// `ptr` is always aligned to the system page size.
/// `len` is not guaranteed to be page-aligned.
///
/// On allocation:
/// - If `len` is smaller than the system page size, only `ptr` is
///   page-aligned.
/// - Otherwise allocation might use huge pages and if it succeeds, both `ptr` and `len` are
///   aligned to the system page size.
pub struct PageAlignedMemory {
    ptr: NonNull<u8>,
    len: usize,
    allocation_method: AllocationMethod,
}

impl PageAlignedMemory {
    /// Allocate memory buffer optimized for io_uring operations, i.e.
    /// using HugeTable when it is available on the host.
    pub fn new(size: usize) -> Result<Self, AllocError> {
        let page_size = Self::page_size();
        if size > page_size {
            let size = size.next_power_of_two();
            if let Ok(alloc) = PageAlignedMemory::alloc_huge_table(size, page_size) {
                log::info!("obtained hugetable io_uring buffer (len={size})");
                return Ok(alloc);
            }
        }

        let layout = Layout::from_size_align(size, page_size)?;
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

    fn alloc_huge_table(memory_size: usize, page_size: usize) -> Result<Self, AllocError> {
        debug_assert!(memory_size.is_power_of_two());
        debug_assert!(page_size.is_power_of_two());
        let aligned_size = memory_size.next_multiple_of(page_size);

        // Safety:
        // doing an ANONYMOUS alloc. addr=NULL is ok, fd is not used.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                aligned_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
                -1,
                0,
            )
        };

        if std::ptr::eq(ptr, libc::MAP_FAILED) {
            return Err(AllocError::MMapFailed);
        }

        Ok(Self {
            ptr: NonNull::new(ptr as *mut u8).ok_or(AllocError::OutOfMemory)?,
            len: aligned_size,
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
                // ptr is a valid pointer returned by mmap
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
    size: usize,
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
        chunk_size: usize,
        registered_buffer: bool,
    ) -> impl Iterator<Item = Self> + use<'_> {
        assert!(
            buffer.len() / FIXED_BUFFER_LEN <= u16::MAX as usize,
            "buffer too large to register in io_uring"
        );
        let buf_start = buffer.as_ptr() as usize;

        buffer.chunks_exact_mut(chunk_size).map(move |buf| {
            let io_buf_index = (buf.as_ptr() as usize - buf_start) / FIXED_BUFFER_LEN;
            Self {
                ptr: buf.as_mut_ptr(),
                size: buf.len(),
                registered_io_buf_index: registered_buffer.then_some(io_buf_index as u16),
            }
        })
    }

    pub fn len(&self) -> usize {
        self.size
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

    /// Return a clone of `self` reduced to specified `size`
    pub fn into_shrinked(self, size: usize) -> Self {
        assert!(size <= self.size);
        Self {
            ptr: self.ptr,
            size,
            registered_io_buf_index: self.registered_io_buf_index,
        }
    }

    /// Register provided buffer as fixed buffer in `io_uring`.
    pub unsafe fn register<S, E: RingOp<S>>(
        buffer: &mut [u8],
        ring: &Ring<S, E>,
    ) -> io::Result<()> {
        let iovecs = buffer
            .chunks(FIXED_BUFFER_LEN)
            .map(|buf| libc::iovec {
                iov_base: buf.as_ptr() as _,
                iov_len: buf.len(),
            })
            .collect::<Vec<_>>();
        unsafe { ring.register_buffers(&iovecs) }
    }
}

impl AsRef<[u8]> for IoBufferChunk {
    fn as_ref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr, self.size) }
    }
}
