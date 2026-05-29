//! Aligned byte buffer with a thread-local pool for reuse.
//!
//! The BPF VM input region (the buffer holding serialized instruction
//! parameters) is allocated fresh on every program invocation in
//! [`crate::serialization::serialize_parameters`]. On a live validator this
//! is the single largest source of page faults — ~36% of all faults — because
//! the buffer grows large enough that jemalloc obtains it via `mmap` and
//! returns it to the OS on free, faulting on every first touch the next time.
//!
//! `solana_sbpf::AlignedMemory` cannot be cleared or reset (its internal
//! length is private and `debug_assert!`s against shrinking), so this module
//! provides a minimal local equivalent that supports `clear` + `reserve` and
//! ships with a per-thread pool. Each [`serialize_parameters`] call checks
//! out a buffer; on drop the buffer returns to the pool with its capacity
//! intact, so subsequent invocations reuse the same backing allocation and
//! no longer fault.
//!
//! The buffer is `HOST_ALIGN`-aligned (16 bytes) to match
//! `solana_sbpf`'s requirement on the BPF input region.
//!
//! [`serialize_parameters`]: crate::serialization::serialize_parameters

use {
    solana_sbpf::{aligned_memory::Pod, ebpf::HOST_ALIGN},
    std::{
        alloc::{Layout, alloc, dealloc, handle_alloc_error},
        cell::RefCell,
        mem::{ManuallyDrop, size_of},
        ops::{Deref, DerefMut},
        ptr::NonNull,
    },
};

/// Upper bound on pooled buffers per thread. A SVM worker only holds a few
/// buffers at once (one per CPI nesting level, bounded by
/// `MAX_INSTRUCTION_STACK_HEIGHT`), so this cap is generous.
const POOL_CAPACITY: usize = 16;

/// Owned `HOST_ALIGN`-aligned byte buffer with separate length and capacity,
/// supporting `clear` / `reserve` (which `solana_sbpf::AlignedMemory` does
/// not). Used as the BPF VM input region in
/// [`crate::serialization::serialize_parameters`].
pub struct AlignedBuffer {
    /// Dangling when `capacity == 0`; otherwise a `HOST_ALIGN`-aligned
    /// allocation of `capacity` bytes.
    ptr: NonNull<u8>,
    len: usize,
    capacity: usize,
}

// SAFETY: `AlignedBuffer` owns its allocation uniquely; transferring it
// across threads is safe.
unsafe impl Send for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocates a new buffer with `capacity` bytes, length 0.
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            return Self {
                ptr: NonNull::dangling(),
                len: 0,
                capacity: 0,
            };
        }
        let layout = Self::layout(capacity);
        // SAFETY: `layout` is non-zero size and a valid power-of-two alignment.
        let ptr = unsafe { alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| handle_alloc_error(layout));
        Self {
            ptr,
            len: 0,
            capacity,
        }
    }

    fn layout(capacity: usize) -> Layout {
        Layout::from_size_align(capacity, HOST_ALIGN).expect("AlignedBuffer layout")
    }

    /// Number of bytes written.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Allocated capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns `true` when no bytes have been written.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reset the write position to 0. Retains the allocated capacity.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Ensures `capacity() >= new_capacity`, reallocating if necessary. Does
    /// not change `len()`.
    pub fn reserve(&mut self, new_capacity: usize) {
        if new_capacity <= self.capacity {
            return;
        }
        let new_layout = Self::layout(new_capacity);
        // SAFETY: layout is valid; we copy the existing initialized prefix
        // and free the old allocation.
        unsafe {
            let new_ptr = alloc(new_layout);
            let new_ptr = NonNull::new(new_ptr).unwrap_or_else(|| handle_alloc_error(new_layout));
            if self.capacity > 0 {
                std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), new_ptr.as_ptr(), self.len);
                dealloc(self.ptr.as_ptr(), Self::layout(self.capacity));
            }
            self.ptr = new_ptr;
            self.capacity = new_capacity;
        }
    }

    /// View the written prefix.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `len <= capacity`; bytes are initialized by prior writes.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// View the written prefix mutably.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: see `as_slice`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Append `value` and advance the length by `size_of::<T>()`.
    ///
    /// # Safety
    /// Caller must ensure `self.len() + size_of::<T>() <= self.capacity()`.
    /// Matches `solana_sbpf::AlignedMemory::write_unchecked`.
    pub unsafe fn write_unchecked<T: Pod>(&mut self, value: T) {
        let pos = self.len;
        let new_len = pos.saturating_add(size_of::<T>());
        debug_assert!(new_len <= self.capacity);
        // SAFETY: caller guaranteed capacity; pointer is aligned to HOST_ALIGN
        // which is at least 8, sufficient for any `Pod` integer we write.
        unsafe {
            self.ptr
                .as_ptr()
                .add(pos)
                .cast::<T>()
                .write_unaligned(value);
        }
        self.len = new_len;
    }

    /// Append all bytes in `value` and advance the length.
    ///
    /// # Safety
    /// Caller must ensure `self.len() + value.len() <= self.capacity()`.
    /// Matches `solana_sbpf::AlignedMemory::write_all_unchecked`.
    pub unsafe fn write_all_unchecked(&mut self, value: &[u8]) {
        let pos = self.len;
        let new_len = pos.saturating_add(value.len());
        debug_assert!(new_len <= self.capacity);
        // SAFETY: caller guaranteed capacity; non-overlapping copy from
        // external slice into our owned allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(
                value.as_ptr(),
                self.ptr.as_ptr().add(pos),
                value.len(),
            );
        }
        self.len = new_len;
    }

    /// Fill `num` bytes with `value` and advance the length.
    /// Returns an error if the write would exceed capacity.
    pub fn fill_write(&mut self, num: usize, value: u8) -> std::io::Result<()> {
        let new_len = self.len.checked_add(num).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "fill_write overflow")
        })?;
        if new_len > self.capacity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fill_write past capacity",
            ));
        }
        // SAFETY: bounds checked above.
        unsafe {
            std::ptr::write_bytes(self.ptr.as_ptr().add(self.len), value, num);
        }
        self.len = new_len;
        Ok(())
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        if self.capacity == 0 {
            return;
        }
        // SAFETY: `ptr` was allocated with this layout in `with_capacity` /
        // `reserve` and is not aliased.
        unsafe {
            dealloc(self.ptr.as_ptr(), Self::layout(self.capacity));
        }
    }
}

thread_local! {
    static POOL: RefCell<Vec<AlignedBuffer>> = const { RefCell::new(Vec::new()) };
}

/// A pooled [`AlignedBuffer`]. Checked out of a thread-local free-list on
/// construction; returns to the pool on drop with capacity preserved, so
/// later checkouts on the same thread reuse the same allocation and avoid
/// page-faulting the BPF VM input region every transaction.
pub struct PooledAlignedBuffer {
    inner: ManuallyDrop<AlignedBuffer>,
}

impl PooledAlignedBuffer {
    /// Check out a buffer with at least `min_capacity` bytes. If the pool
    /// has a buffer with sufficient capacity, it is cleared and returned;
    /// otherwise a buffer is allocated (and may grow via `reserve` after
    /// checkout if the caller needs more).
    pub fn checkout(min_capacity: usize) -> Self {
        let buf = POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            // Pick the first buffer that fits — pool entries are small so
            // a linear scan is cheap.
            if let Some(idx) = pool.iter().position(|b| b.capacity() >= min_capacity) {
                let mut b = pool.swap_remove(idx);
                b.clear();
                b
            } else {
                AlignedBuffer::with_capacity(min_capacity)
            }
        });
        Self {
            inner: ManuallyDrop::new(buf),
        }
    }
}

impl Deref for PooledAlignedBuffer {
    type Target = AlignedBuffer;
    fn deref(&self) -> &AlignedBuffer {
        &self.inner
    }
}

impl DerefMut for PooledAlignedBuffer {
    fn deref_mut(&mut self) -> &mut AlignedBuffer {
        &mut self.inner
    }
}

impl Drop for PooledAlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: `self.inner` is not used after this line; we move it out
        // and either push to the pool or let it drop normally.
        let buf = unsafe { ManuallyDrop::take(&mut self.inner) };
        POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < POOL_CAPACITY {
                pool.push(buf);
            }
            // Otherwise the pool is full; let `buf` drop and free its memory.
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_clear() {
        let mut b = AlignedBuffer::with_capacity(32);
        assert_eq!(b.len(), 0);
        assert_eq!(b.capacity(), 32);
        unsafe {
            b.write_unchecked::<u64>(0xdead_beef_u64.to_le());
            b.write_all_unchecked(&[1, 2, 3, 4]);
        }
        b.fill_write(8, 0).unwrap();
        assert_eq!(b.len(), 20);
        b.clear();
        assert_eq!(b.len(), 0);
        // The allocation is retained.
        assert_eq!(b.capacity(), 32);
    }

    #[test]
    fn reserve_preserves_data() {
        let mut b = AlignedBuffer::with_capacity(8);
        unsafe {
            b.write_all_unchecked(&[1, 2, 3, 4]);
        }
        b.reserve(64);
        assert!(b.capacity() >= 64);
        assert_eq!(b.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn fill_write_rejects_overflow() {
        let mut b = AlignedBuffer::with_capacity(4);
        assert!(b.fill_write(5, 0).is_err());
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn pool_reuses_allocation() {
        let ptr_first;
        {
            let mut buf = PooledAlignedBuffer::checkout(128);
            unsafe { buf.write_all_unchecked(&[7u8; 16]) };
            ptr_first = buf.as_slice().as_ptr();
        }
        let buf = PooledAlignedBuffer::checkout(128);
        assert_eq!(buf.len(), 0);
        // After checkout the pool returned the same backing allocation.
        // Use the raw pointer of the underlying buffer (length is 0 so
        // as_slice() yields an empty slice; we test the dangling-or-real
        // pointer of the storage directly).
        assert_eq!(buf.inner.ptr.as_ptr(), ptr_first as *mut u8);
    }

    #[test]
    fn alignment_is_host_align() {
        let buf = AlignedBuffer::with_capacity(1);
        assert_eq!(buf.ptr.as_ptr() as usize % HOST_ALIGN, 0);
    }
}
