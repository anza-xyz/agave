//! [`Reader`] and [`Writer`] implementations.
#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use {
    crate::error::{read_size_limit, write_size_limit, writer_trailing_bytes, Result},
    core::{mem::MaybeUninit, ptr, slice},
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

    /// Copy exactly `size_of::<T>()` bytes from the [`Reader`] into `dst`.
    ///
    /// # Safety
    ///
    /// - `T` must be initialized by reads of `size_of::<T>()` bytes.
    #[inline]
    pub unsafe fn read_t<T>(&mut self, dst: &mut MaybeUninit<T>) -> Result<()> {
        self.read_slice_t(slice::from_mut(dst))
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
        self.read_t(&mut val)?;
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
    buffer: &'a mut [MaybeUninit<u8>],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub fn new(buffer: &'a mut [MaybeUninit<u8>]) -> Self {
        Self { buffer, pos: 0 }
    }

    #[cfg(feature = "alloc")]
    pub fn from_vec(buffer: &'a mut Vec<u8>) -> Self {
        Self {
            pos: buffer.len(),
            buffer: buffer.spare_capacity_mut(),
        }
    }

    #[inline]
    fn ensure_capacity(&mut self, len: usize) -> Result<()> {
        if self.buffer.len().saturating_sub(self.pos) < len {
            return Err(write_size_limit(len));
        }
        Ok(())
    }

    /// Get the number of bytes written to the buffer.
    #[inline]
    pub fn finish(self) -> usize {
        self.pos
    }

    /// Get the number of bytes written to the buffer, and error if there are trailing bytes.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn finish_disallow_trailing_bytes(self) -> Result<usize> {
        if self.pos != self.buffer.len() {
            return Err(writer_trailing_bytes(
                self.buffer.len().saturating_sub(self.pos),
            ));
        }
        Ok(self.pos)
    }

    /// Write exactly `src.len()` bytes from the given `src` into the internal buffer.
    #[inline]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn write_exact(&mut self, src: &[u8]) -> Result<()> {
        self.ensure_capacity(src.len())?;

        unsafe {
            // SAFETY:
            // - `src` mustn't overlap with the internal buffer (shouldn't be the case unless the user is doing something they shouldn't).
            // - We just checked that we have enough capacity in the internal buffer.
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.buffer.as_mut_ptr().add(self.pos).cast(),
                src.len(),
            );
        }
        self.pos += src.len();
        Ok(())
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
    #[allow(clippy::arithmetic_side_effects)]
    pub unsafe fn write_with<F>(&mut self, len: usize, write: F) -> Result<()>
    where
        F: FnOnce(&mut [MaybeUninit<u8>]) -> Result<()>,
    {
        self.ensure_capacity(len)?;
        let dst = &mut self.buffer[self.pos..self.pos + len];
        write(dst)?;
        self.pos += len;
        Ok(())
    }

    /// Write T into the internal buffer.
    ///
    /// # Safety
    ///
    /// - `T` must be plain ol' data.
    #[inline]
    pub unsafe fn write_t<T>(&mut self, value: &T) -> Result<()> {
        self.write_slice_t(slice::from_ref(value))
    }

    /// Write `len` bytes from the given `write` function into the internal buffer.
    ///
    /// # Safety
    ///
    /// - `write` must write EXACTLY `len` bytes into the given buffer.
    /// - `write` must not write from a buffer that overlap with the internal buffer.
    #[inline]
    pub unsafe fn write_slice_t<T>(&mut self, value: &[T]) -> Result<()> {
        unsafe {
            self.write_exact(slice::from_raw_parts(
                value.as_ptr().cast(),
                size_of_val(value),
            ))
        }
    }
}
