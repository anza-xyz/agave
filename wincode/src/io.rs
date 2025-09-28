//! [`Reader`] and [`Writer`] implementations.
use {
    crate::error::{read_size_limit, write_size_limit, Result},
    std::{mem::MaybeUninit, ptr},
};

/// In-memory reader that allows direct reads from the source buffer
/// into user given destination buffers.
pub struct Reader<'a> {
    cursor: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { cursor: bytes }
    }

    /// Copy exactly `len` bytes from the [`Reader`] into `buf`.
    ///
    /// # Safety
    ///
    /// - `buf` must not overlap with the cursor.
    /// - `buf` must be valid for writes of `len` bytes.
    #[inline(always)]
    pub unsafe fn read_exact(&mut self, buf: *mut u8, len: usize) -> Result<()> {
        let Some((src, rest)) = self.cursor.split_at_checked(len) else {
            return Err(read_size_limit(len));
        };
        unsafe {
            // SAFETY:
            // - `src` is a valid pointer to a `[u8]`.
            // - `src` is valid for reads of `len` bytes (given `split_at_checked`).
            // - Caller ensures `buf` is valid for writes of `len` bytes.
            ptr::copy_nonoverlapping(src.as_ptr(), buf, len);
        }
        self.cursor = rest;
        Ok(())
    }

    /// Copy exactly `size_of::<T>()` bytes from the [`Reader`] into `ptr`.
    ///
    /// # Safety
    ///
    /// - `ptr` must not overlap with the cursor.
    /// - `ptr` must be valid for writes of `size_of::<T>()` bytes.
    /// - `T` must be plain ol' data.
    #[inline]
    pub unsafe fn read_t<T>(&mut self, ptr: *mut T) -> Result<()> {
        unsafe { self.read_exact(ptr as *mut u8, size_of::<T>()) }
    }

    /// Read T from the cursor into a new T.
    ///
    /// # Safety
    ///
    /// - `T` must be plain ol' data.
    /// - `T` must be initialized by reads of `size_of::<T>()` bytes.
    #[inline(always)]
    pub unsafe fn get_t<T>(&mut self) -> Result<T> {
        let mut val = MaybeUninit::<T>::uninit();
        unsafe {
            // SAFETY:
            // - `val` is a valid pointer to a `T`.
            // - Caller ensures `T` is plain ol' data and is initialized by reads of `size_of::<T>()` bytes.
            self.read_exact(val.as_mut_ptr() as *mut u8, size_of::<T>())?;
        }
        Ok(val.assume_init())
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.cursor
    }

    /// Advance the cursor by `amt` bytes without checking bounds.
    #[inline(always)]
    pub fn consume_unchecked(&mut self, amt: usize) {
        self.cursor = &self.cursor[amt..];
    }

    /// Advance `amt` bytes from the reader and discard them.
    #[inline]
    pub fn consume(&mut self, amt: usize) -> Result<()> {
        if self.cursor.len() < amt {
            return Err(read_size_limit(amt));
        };
        self.consume_unchecked(amt);
        Ok(())
    }
}

/// In-memory writer that allows direct writes from user given buffers
/// into the internal destination buffer.
pub struct Writer<'a> {
    buffer: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    pub fn new(buffer: &'a mut Vec<u8>) -> Self {
        Self { buffer }
    }

    /// Write exactly `len` bytes from the given `buf` into the internal buffer.
    ///
    /// # Safety
    ///
    /// - `buf` must not overlap with the internal buffer.
    /// - `buf` must be valid for reads of `len` bytes.
    #[inline(always)]
    pub unsafe fn write_exact(&mut self, buf: *const u8, len: usize) -> Result<()> {
        let self_len = self.ensure_capacity_for(len)?;
        unsafe {
            // SAFETY:
            // - Caller ensures `buf` is valid for reads of `len` bytes.
            // - Caller ensures `buf` does not overlap with the internal buffer.
            // - `self.buffer` is valid for writes of `len` bytes (given `capacity` check).
            ptr::copy_nonoverlapping(buf, self.buffer.as_mut_ptr().add(self_len), len);
            // SAFETY: capacity was checked above and we just wrote `len` bytes.
            #[allow(clippy::arithmetic_side_effects)]
            self.buffer.set_len(self_len + len);
        }
        Ok(())
    }

    #[inline(always)]
    fn ensure_capacity_for(&self, len: usize) -> Result<usize> {
        let buf_len = self.buffer.len();
        if len > self.buffer.capacity().saturating_sub(buf_len) {
            return Err(write_size_limit(len));
        }
        Ok(buf_len)
    }

    /// Write `len` bytes from the given `write` function into the internal buffer.
    ///
    /// Prefer [`Writer::write_exact`] or [`Writer::write_t`] wherever possible.
    ///
    /// This method can be used to get `len` [`MaybeUninit<u8>`] bytes from internal
    /// buffer memory and write into them directly.
    ///
    /// # Safety
    ///
    /// - `write` must write EXACTLY `len` bytes into the given buffer.
    /// - `write` must not write from a buffer that overlap with the internal buffer.
    pub unsafe fn write_with<F>(&mut self, len: usize, write: F) -> Result<()>
    where
        F: FnOnce(&mut [MaybeUninit<u8>]) -> Result<()>,
    {
        let self_len = self.ensure_capacity_for(len)?;
        let buf = &mut self.buffer.spare_capacity_mut()[..len];
        write(buf)?;
        unsafe {
            // SAFETY: Caller ensures `write` writes exactly `len` bytes.
            #[allow(clippy::arithmetic_side_effects)]
            self.buffer.set_len(self_len + len);
        }
        Ok(())
    }

    /// Write T into the internal buffer.
    ///
    /// # Safety
    ///
    /// - `T` must be plain ol' data.
    #[inline(always)]
    pub unsafe fn write_t<T>(&mut self, value: &T) -> Result<()> {
        unsafe { self.write_exact(value as *const T as *const u8, size_of::<T>()) }
    }
}
