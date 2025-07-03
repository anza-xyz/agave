use std::{
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    slice,
};

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
            Self::HugeTable(ref mut mem) => mem.deref_mut(),
        }
    }
}

impl AsMut<[u8]> for LargeBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Vec(vec) => vec.as_mut_slice(),
            LargeBuffer::HugeTable(ref mut mem) => mem,
        }
    }
}

impl LargeBuffer {
    /// Allocare memory buffer optimized for io_uring operations, i.e.
    /// using HugeTable when it is available on the host.
    pub fn new(size: usize) -> Self {
        if size > PageAlignedMemory::page_size() {
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

/// Fixed mutable view into externally allocated bytes buffer
///
/// It is an unsafe (no lifetime tracking) equivalent of `&mut [u8]`
pub struct BorrowedBytesMut {
    ptr: *mut u8,
    size: usize,
}

impl BorrowedBytesMut {
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            size: 0,
        }
    }

    pub fn from_mut_slice(buf: &mut [u8]) -> Self {
        Self {
            ptr: buf.as_mut_ptr(),
            size: buf.len(),
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.size
    }

    /// Return a clone of `self` reduced to specified `size`
    pub fn sub_buf_to(&self, size: usize) -> Self {
        assert!(size <= self.size);
        Self {
            ptr: self.ptr,
            size,
        }
    }
}

impl AsRef<[u8]> for BorrowedBytesMut {
    fn as_ref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr, self.size) }
    }
}
