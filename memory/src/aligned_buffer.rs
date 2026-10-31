use {
    crate::{AllocOptions, PageAllocator, page_size},
    allocator_api2::alloc::{Allocator, Layout},
    std::{
        io,
        ops::{Deref, DerefMut},
        ptr::NonNull,
        slice,
    },
};

/// Zeroed byte buffer with the size and alignment of a [`Layout`], and configurable page backing.
///
/// Unlike `Box<[u8]>`, which guarantees only byte alignment, it suits memory that the kernel or a
/// device accesses directly, such as direct I/O buffers requiring block alignment. By default,
/// buffers spanning at least one huge page get transparent huge pages to cut page faults and TLB
/// misses.
///
/// ```
/// use agave_memory::{AlignedBuffer, page_size};
///
/// let buf = AlignedBuffer::new_page_aligned(16 * 1024 * 1024)?;
/// assert!(buf.as_ptr().addr().is_multiple_of(page_size()));
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
    alloc: PageAllocator,
}

// SAFETY: `AlignedBuffer` exclusively owns the allocation behind its raw `ptr`, and frees it using
// `PageAllocator` from any thread.
unsafe impl Send for AlignedBuffer {}
// SAFETY: shared references only read the bytes behind `ptr`, and writes through `as_mut_ptr`
// need unsafe code, which must rule out data races.
unsafe impl Sync for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocates a zeroed buffer for `layout` with the default [`AllocOptions`] backing.
    ///
    /// Fails with [`io::ErrorKind::OutOfMemory`] when the allocation fails.
    pub fn new(layout: Layout) -> io::Result<Self> {
        Self::new_with_options(layout, AllocOptions::default())
    }

    /// Allocates a zeroed buffer of `len` bytes aligned to [`page_size`], with the default
    /// [`AllocOptions`] backing.
    ///
    /// Fails with [`io::ErrorKind::InvalidInput`] when `len` exceeds the largest [`Layout`].
    pub fn new_page_aligned(len: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(len, page_size())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Self::new(layout)
    }

    /// Allocates a zeroed buffer for `layout` with the backing specified by `options`.
    ///
    /// Fails with [`io::ErrorKind::OutOfMemory`] when the allocation fails.
    pub fn new_with_options(layout: Layout, options: AllocOptions) -> io::Result<Self> {
        let alloc = PageAllocator::new(options);
        let ptr = alloc
            .allocate_zeroed(layout)
            .map_err(|_| io::ErrorKind::OutOfMemory)?;
        Ok(Self {
            ptr: ptr.cast(),
            layout,
            alloc,
        })
    }

    /// Returns a pointer valid for writes, for buffers shared with the kernel or other threads.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: `new` allocated `ptr` from `alloc` with `layout`.
        unsafe { self.alloc.deallocate(self.ptr, self.layout) }
    }
}

impl Deref for AlignedBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `ptr` holds `layout.size()` initialized bytes.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `ptr` holds `layout.size()` initialized bytes, and `&mut self` makes the access
        // unique.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{HUGE_PAGE_SIZE, HugePages},
    };

    fn new_buffer(len: usize, align: usize, options: AllocOptions) -> AlignedBuffer {
        let layout = Layout::from_size_align(len, align).unwrap();
        let mut buf = AlignedBuffer::new_with_options(layout, options).unwrap();
        assert!(buf.as_ptr().addr().is_multiple_of(align));
        assert_eq!(buf.len(), len);
        assert!(buf.iter().all(|&b| b == 0));
        buf.fill(1);
        buf
    }

    #[test]
    fn test_buffer_has_layout_alignment() {
        for options in [
            AllocOptions::new().huge_pages(HugePages::Off),
            AllocOptions::new(),
        ] {
            new_buffer(4097, 64, options);
            new_buffer(HUGE_PAGE_SIZE - 1, page_size(), options);
            new_buffer(3 * HUGE_PAGE_SIZE + 4097, page_size(), options);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_large_buffer_starts_at_huge_page() {
        let buf = AlignedBuffer::new_page_aligned(3 * HUGE_PAGE_SIZE + 4097).unwrap();
        assert!(buf.as_ptr().addr().is_multiple_of(HUGE_PAGE_SIZE));
    }
}
