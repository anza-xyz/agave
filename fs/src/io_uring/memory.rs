use {
    agave_io_uring::{Ring, RingOp},
    std::{
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

pub enum LargeBuffer {
    Vec(Vec<u8>),
    HugeTable(PageAlignedMemory),
}

impl Deref for LargeBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Vec(buf) => buf.as_slice(),
            Self::HugeTable(mem) => mem.deref(),
        }
    }
}

impl DerefMut for LargeBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Vec(buf) => buf.as_mut_slice(),
            Self::HugeTable(mem) => mem.deref_mut(),
        }
    }
}

impl AsMut<[u8]> for LargeBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Vec(vec) => vec.as_mut_slice(),
            LargeBuffer::HugeTable(mem) => mem,
        }
    }
}

impl LargeBuffer {
    /// Allocate memory buffer optimized for io_uring operations, i.e.
    /// using HugeTable when it is available on the host.
    pub fn new(size: usize) -> Self {
        if size > PageAlignedMemory::page_size() {
            let size = size.next_power_of_two();
            if let Ok(alloc) = PageAlignedMemory::alloc_huge_table(size) {
                log::info!("obtained hugetable io_uring buffer (len={size})");
                return Self::HugeTable(alloc);
            }
        }
        Self::Vec(vec![0; size])
    }
}

#[derive(Debug)]
struct AllocError;

pub struct PageAlignedMemory {
    ptr: NonNull<u8>,
    len: usize,
}

impl PageAlignedMemory {
    fn alloc_huge_table(memory_size: usize) -> Result<Self, AllocError> {
        let page_size = Self::page_size();
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
            return Err(AllocError);
        }

        Ok(Self {
            ptr: NonNull::new(ptr as *mut u8).ok_or(AllocError)?,
            len: aligned_size,
        })
    }

    fn page_size() -> usize {
        // Safety: just a libc wrapper
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }
}

impl Drop for PageAlignedMemory {
    fn drop(&mut self) {
        // Safety:
        // ptr is a valid pointer returned by mmap
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len);
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

/// Fixed mutable view into externally allocated IO bytes buffer
/// registered in `io_uring` for access in scheduled IO operations.
///
/// It is used as an unsafe (no lifetime tracking) equivalent of `&mut [u8]`.
#[derive(Debug)]
pub(super) struct FixedIoBuffer {
    ptr: *mut u8,
    size: usize,
    io_buf_index: Option<u16>,
}

impl FixedIoBuffer {
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            size: 0,
            io_buf_index: None,
        }
    }

    /// Split buffer into `chunk_size` sized `IoFixedBuffer` buffers for use as registered
    /// buffer in io_uring operations.
    #[allow(clippy::arithmetic_side_effects)]
    pub unsafe fn split_buffer_chunks(
        buffer: &mut [u8],
        chunk_size: usize,
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
                io_buf_index: Some(io_buf_index as u16),
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
        self.io_buf_index
    }

    /// Return a clone of `self` reduced to specified `size`
    pub fn into_shrinked(self, size: usize) -> Self {
        assert!(size <= self.size);
        Self {
            ptr: self.ptr,
            size,
            io_buf_index: self.io_buf_index,
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

impl AsRef<[u8]> for FixedIoBuffer {
    fn as_ref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr, self.size) }
    }
}
