use {
    agave_scheduler_bindings::SharablePubkeys, rts_alloc::Allocator, solana_pubkey::Pubkey,
    std::ptr::NonNull,
};

pub struct PubkeysPtr<'a> {
    ptr: NonNull<Pubkey>,
    count: usize,
    allocator: &'a Allocator,
}

impl<'a> PubkeysPtr<'a> {
    /// Constructs the pointer from a [`SharablePubkeys`].
    ///
    /// # Safety
    ///
    /// - The allocation pointed to by this region must not have previously been freed.
    /// - Pointer must be exclusive so that calling [`Self::free`] is safe.
    /// - `sharable_pubkeys.num_pubkeys` must be accurate and not overrun the allocation.
    pub unsafe fn from_sharable_pubkeys(
        sharable_pubkeys: &SharablePubkeys,
        allocator: &'a Allocator,
    ) -> Self {
        assert_ne!(sharable_pubkeys.num_pubkeys, 0);
        let ptr = allocator.ptr_from_offset(sharable_pubkeys.offset).cast();

        Self {
            ptr,
            count: sharable_pubkeys.num_pubkeys as usize,
            allocator,
        }
    }

    /// Returns the allocation as a slice.
    pub fn as_slice(&self) -> &[Pubkey] {
        // SAFETY
        // - Constructor invariants guarantee that we don't overrun the end of the allocation.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }
    }

    /// Frees the underlying allocation.
    pub fn free(self) {
        // SAFETY
        // - Constructor invariants guarantee that we exclusively own this pointer.
        unsafe { self.allocator.free(self.ptr.cast()) };
    }
}
