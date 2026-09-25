use {
    agave_scheduler_bindings::SharablePubkeys, core::ptr::NonNull, rts_alloc::Allocator,
    solana_pubkey::Pubkey,
};

#[derive(Debug)]
pub struct PubkeysPtr {
    ptr: NonNull<Pubkey>,
    count: usize,
}

impl PubkeysPtr {
    /// Constructions a [`PubkeysPtr`] from raw parts.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid for reads.
    /// - `count` must be accurate (in number of pubkeys) and not overrun the end of `ptr`.
    ///
    /// # Note
    ///
    /// If you are trying to construct a pointer for use by Agave, you almost certainly want to use
    /// [`Self::from_sharable_pubkeys`].
    pub unsafe fn from_raw_parts(ptr: NonNull<Pubkey>, count: usize) -> Self {
        Self { ptr, count }
    }

    /// Constructs the pointer from a [`SharablePubkeys`].
    ///
    /// # Safety
    ///
    /// - `sharable_pubkeys.offset` must have been allocated by `allocator`.
    /// - The allocation pointed to by this region must not have previously been freed.
    /// - Pointer must be exclusive so that calling [`Self::free`] is safe.
    /// - `sharable_pubkeys.num_pubkeys` must be accurate and not overrun the allocation.
    pub unsafe fn from_sharable_pubkeys(
        sharable_pubkeys: &SharablePubkeys,
        allocator: &Allocator,
    ) -> Self {
        assert_ne!(sharable_pubkeys.num_pubkeys, 0);
        // SAFETY: `sharable_pubkeys.offset` was allocated by `allocator`.
        let ptr = unsafe { allocator.ptr_from_offset(sharable_pubkeys.offset) }.cast();

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

/// Frees resolved addresses unless ownership is transferred with [`Self::into_inner`].
pub struct OwnedPubkeysPtr<'a> {
    pubkeys: Option<PubkeysPtr>,
    allocator: &'a Allocator,
}

impl<'a> OwnedPubkeysPtr<'a> {
    /// # Safety
    /// Any nonempty region must contain initialized pubkeys in an allocation exclusively owned
    /// by the caller in `allocator`. The caller transfers ownership to this guard.
    pub unsafe fn from_region(region: Option<SharablePubkeys>, allocator: &'a Allocator) -> Self {
        let pubkeys = region
            .filter(|region| region.num_pubkeys != 0)
            .map(|region| {
                // SAFETY: the caller guarantees a valid nonempty allocation.
                unsafe { PubkeysPtr::from_sharable_pubkeys(&region, allocator) }
            });
        Self { pubkeys, allocator }
    }

    /// Borrows the pubkeys, or an empty slice when there is no allocation.
    pub fn as_slice(&self) -> &[Pubkey] {
        self.pubkeys.as_ref().map_or(&[], PubkeysPtr::as_slice)
    }

    /// Transfers the pointer to the caller without freeing its allocation.
    pub fn into_inner(mut self) -> Option<PubkeysPtr> {
        self.pubkeys.take()
    }
}

impl Drop for OwnedPubkeysPtr<'_> {
    fn drop(&mut self) {
        if let Some(pubkeys) = self.pubkeys.take() {
            // SAFETY: the guard exclusively owns this live allocation.
            unsafe { pubkeys.free(self.allocator) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_pubkeys_free_unless_transferred() {
        let file = tempfile::tempfile().unwrap();
        // SAFETY: this fresh file is initialized exactly once.
        let allocator = unsafe { Allocator::create(&file, 4 * 1024 * 1024, 1, 65536) }.unwrap();
        let key = Pubkey::from([7; 32]);
        let ptr = allocator.allocate(size_of::<Pubkey>() as u32).unwrap();
        // SAFETY: the allocation is aligned and sized for a pubkey.
        unsafe { ptr.cast::<Pubkey>().write(key) };
        let region = SharablePubkeys {
            // SAFETY: this pointer was allocated by this allocator.
            offset: unsafe { allocator.offset(ptr) },
            num_pubkeys: 1,
        };
        // SAFETY: this test transfers exclusive ownership of the initialized allocation.
        let owned = unsafe { OwnedPubkeysPtr::from_region(Some(region), &allocator) };
        assert_eq!(owned.as_slice(), &[key]);
        let pubkeys = owned.into_inner().unwrap();
        assert_eq!(pubkeys.as_slice(), &[key]);
        assert_ne!(allocator.outstanding_allocation_bytes(), 0);
        // SAFETY: into_inner returned ownership of this still-live allocation.
        let owned = unsafe { OwnedPubkeysPtr::from_region(Some(region), &allocator) };
        drop(owned);
        assert_eq!(allocator.outstanding_allocation_bytes(), 0);

        // SAFETY: neither absent nor zero-length regions describe allocations.
        let empty = unsafe { OwnedPubkeysPtr::from_region(None, &allocator) };
        assert!(empty.as_slice().is_empty());
        assert!(empty.into_inner().is_none());
        let empty = SharablePubkeys {
            offset: usize::MAX,
            num_pubkeys: 0,
        };
        // SAFETY: a zero-length region has no allocation to access or free.
        let empty = unsafe { OwnedPubkeysPtr::from_region(Some(empty), &allocator) };
        drop(empty);
    }
}
