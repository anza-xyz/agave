use {
    agave_scheduler_bindings::SharableTransactionRegion,
    agave_transaction_view::transaction_data::TransactionData, core::ptr::NonNull,
    rts_alloc::Allocator,
};

pub struct TransactionPtr {
    ptr: NonNull<u8>,
    len: usize,
}

impl TransactionData for TransactionPtr {
    fn data(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl TransactionPtr {
    /// # Safety
    /// - `sharable_transaction_region` must reference a valid offset and length
    ///   within the `allocator`.
    pub unsafe fn from_sharable_transaction_region(
        sharable_transaction_region: &SharableTransactionRegion,
        allocator: &Allocator,
    ) -> Self {
        let ptr = allocator.ptr_from_offset(sharable_transaction_region.offset);
        Self {
            ptr,
            len: sharable_transaction_region.length as usize,
        }
    }

    /// Translate the ptr type into a sharable region.
    ///
    /// # Safety
    /// - `allocator` must be the allocator owning the memory region pointed
    ///   to by `self`.
    pub unsafe fn to_sharable_transaction_region(
        &self,
        allocator: &Allocator,
    ) -> SharableTransactionRegion {
        // SAFETY: The `TransactionPtr` creation `Self::from_sharable_transaction_region`
        // is already conditioned on the offset being valid, if that safety constraint
        // was satisfied translation back to offset is safe.
        let offset = unsafe { allocator.offset(self.ptr) };
        SharableTransactionRegion {
            offset,
            length: self.len as u32,
        }
    }

    /// Frees the memory region pointed to in the `allocator`.
    /// This should only be called by the owner of the memory
    /// i.e. the external scheduler.
    ///
    /// # Safety
    /// - Data region pointed to by `TransactionPtr` belongs to the `allocator`.
    /// - Inner `ptr` must not have been previously freed.
    pub unsafe fn free(self, allocator: &Allocator) {
        unsafe { allocator.free(self.ptr) }
    }
}
