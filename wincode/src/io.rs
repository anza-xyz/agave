//! [`Reader`] and [`Writer`] implementations.
use {
    crate::error::{read_size_limit, write_size_limit, Result},
    std::{mem::MaybeUninit, ptr, slice},
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

    /// Copy exactly `dst.len()` bytes from the [`Reader`] into `dst`.
    #[inline]
    pub fn read_exact(&mut self, dst: &mut [MaybeUninit<u8>]) -> Result<()> {
        let Some((src, rest)) = self.cursor.split_at_checked(dst.len()) else {
            return Err(read_size_limit(dst.len()));
        };
        unsafe {
            // SAFETY:
            // - `buf` mustn't overlap with the cursor (shouldn't be the case unless the user is doing something they shouldn't).
            // - We just checked that we have enough bytes remaining in the cursor.
            ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr().cast::<u8>(), dst.len());
        }
        self.cursor = rest;
        Ok(())
    }

    /// Copy exactly `size_of::<T>()` bytes from the [`Reader`] into `ptr`.
    ///
    /// # Safety
    ///
    /// - `T` must be initialized by reads of `size_of::<T>()` bytes.
    #[inline]
    pub unsafe fn read_t<T>(&mut self, dst: &mut MaybeUninit<T>) -> Result<()> {
        let slice = unsafe {
            slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
        };
        self.read_exact(slice)?;
        Ok(())
    }

    /// Copy exactly `dst.len() * size_of::<T>()` bytes from the [`Reader`] into `dst`.
    ///
    /// # Safety
    ///
    /// - `T` must be initialized by reads of `size_of::<T>()` bytes.
    #[inline]
    pub unsafe fn read_slice_t<T>(&mut self, dst: &mut [MaybeUninit<T>]) -> Result<()> {
        let slice = unsafe {
            slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of_val(dst))
        };
        self.read_exact(slice)?;
        Ok(())
    }

    /// Read T from the cursor into a new T.
    ///
    /// # Safety
    ///
    /// - `T` must be initialized by reads of `size_of::<T>()` bytes.
    #[inline(always)]
    pub unsafe fn get_t<T>(&mut self) -> Result<T> {
        let mut val = MaybeUninit::<T>::uninit();
        let slice = unsafe {
            slice::from_raw_parts_mut(val.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
        };
        self.read_exact(slice)?;
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

    /// Write exactly `src.len()` bytes from the given `src` into the internal buffer.
    #[inline]
    pub fn write_exact(&mut self, src: &[u8]) -> Result<()> {
        let self_len = self.ensure_capacity_for(src.len())?;
        unsafe {
            // SAFETY:
            // - `src` mustn't overlap with the internal buffer (shouldn't be the case unless the user is doing something they shouldn't).
            // - We just checked that we have enough capacity in the internal buffer.
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.buffer.as_mut_ptr().add(self_len),
                src.len(),
            );
            // SAFETY: capacity was checked above and we just wrote `len` bytes.
            #[allow(clippy::arithmetic_side_effects)]
            self.buffer.set_len(self_len + src.len());
        }
        Ok(())
    }

    #[inline]
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
    #[inline]
    pub unsafe fn write_t<T>(&mut self, value: &T) -> Result<()> {
        unsafe {
            self.write_exact(slice::from_raw_parts(
                value as *const T as *const u8,
                size_of::<T>(),
            ))
        }
    }

    /// Write `len` bytes from the given `write` function into the internal buffer.
    ///
    /// # Safety
    ///
    /// - `write` must write EXACTLY `len` bytes into the given buffer.
    /// - `write` must not write from a buffer that overlap with the internal buffer.
    #[inline]
    pub unsafe fn write_slice_t<T>(&mut self, value: &[T]) -> Result<()> {
        #[allow(clippy::arithmetic_side_effects)]
        unsafe {
            self.write_exact(slice::from_raw_parts(
                value.as_ptr().cast(),
                size_of_val(value),
            ))
        }
    }
}
