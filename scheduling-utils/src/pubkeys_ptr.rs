use {
    agave_scheduler_bindings::SharablePubkeys, rts_alloc::Allocator, solana_pubkey::Pubkey,
    std::ptr::NonNull,
};

pub struct PubkeysPtr {
    ptr: NonNull<Pubkey>,
    count: usize,
}

impl PubkeysPtr {
    /// Constructs the pointer from a [`SharablePubkeys`].
    ///
    /// # Safety
    ///
    /// - The allocation pointed to by this region must not have previously been freed.
    /// - Pointer must be exclusive so that calling [`Self::free`] is safe.
    /// - `sharable_pubkeys.num_pubkeys` must be accurate and not overrun the allocation.
    pub unsafe fn from_sharable_pubkeys(
        sharable_pubkeys: &SharablePubkeys,
        allocator: &Allocator,
    ) -> Self {
        assert_ne!(sharable_pubkeys.num_pubkeys, 0);
        let ptr = allocator.ptr_from_offset(sharable_pubkeys.offset).cast();

        Self {
            ptr,
            count: sharable_pubkeys.num_pubkeys as usize,
        }
    }

    /// Returns the allocation as a slice.
    pub fn as_slice(&self) -> &[Pubkey] {
        // SAFETY
        // - Constructor invariants guarantee that we don't overrun the end of the allocation.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }
    }

    /// Frees the underlying allocation.
    ///
    /// # Safety
    ///
    /// - `Self` must be exclusively owned.
    pub unsafe fn free(self, allocator: &Allocator) {
        // SAFETY
        // - Caller guarantees that we exclusively own this pointer.
        unsafe { allocator.free(self.ptr.cast()) };
    }
}
