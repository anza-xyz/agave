//! The read of an account's meta entry, shared by the accounts file formats

use std::{mem::MaybeUninit, slice};

/// The size of a `MetaEntryBuffer`: the most a meta entry read is, in any format
pub(crate) const META_ENTRY_BUFFER_SIZE: usize = 4 * 1024;

/// Byte buffer used for reading meta entries.
///
/// This buffer is aligned to ensure it is safe to read
/// the meta entry fields after loading from disk.
#[repr(align(8))]
pub(crate) struct MetaEntryBuffer([MaybeUninit<u8>; META_ENTRY_BUFFER_SIZE]);

impl MetaEntryBuffer {
    pub(crate) fn new() -> Self {
        Self([MaybeUninit::uninit(); META_ENTRY_BUFFER_SIZE])
    }

    /// The buffer's first `len` bytes, to read into
    pub(crate) fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        let bytes = &mut self.0[..len];
        // SAFETY:
        // * bytes is non-null, thus the ptr used for creating the slice is non-null
        // * bytes is len bytes, thus slice is valid for all bytes
        // * buffer is aligned for a meta entry, thus so is the slice
        // * buffer is a single allocation, thus so is the slice
        // * bytes is not referenced while the slice is alive
        // * buffer is META_ENTRY_BUFFER_SIZE, which is less than isize::MAX, thus so is the slice
        unsafe { slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<u8>(), len) }
    }
}
