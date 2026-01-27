use {
    agave_scheduler_bindings::{
        SharableTransactionBatchRegion, SharableTransactionRegion, MAX_TRANSACTIONS_PER_MESSAGE,
    },
    agave_transaction_view::transaction_data::TransactionData,
    core::ptr::NonNull,
    rts_alloc::Allocator,
    std::marker::PhantomData,
};

#[derive(Debug)]
pub struct TransactionPtr {
    ptr: NonNull<u8>,
    count: usize,
}

impl TransactionData for TransactionPtr {
    fn data(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }
    }
}

impl TransactionData for &TransactionPtr {
    fn data(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }
    }
}

impl TransactionPtr {
    /// Constructions a [`TransactionPtr`] from raw parts.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid for reads.
    /// - `count` must be accurate and not overrun the end of `ptr`.
    ///
    /// # Note
    ///
    /// If you are trying to construct a pointer for use by Agave, you almost certainly want to use
    /// [`Self::from_sharable_transaction_region`].
    #[cfg(feature = "dev-context-only-utils")]
    pub unsafe fn from_raw_parts(ptr: NonNull<u8>, count: usize) -> Self {
        Self { ptr, count }
    }

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
            count: sharable_transaction_region.length as usize,
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
            length: self.count as u32,
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

/// A batch of transaction pointers that can be iterated over.
pub struct TransactionPtrBatch<'a, M = ()> {
    tx_ptr: NonNull<SharableTransactionRegion>,
    meta_ptr: NonNull<M>,
    num_transactions: usize,
    allocator: &'a Allocator,

    _meta: PhantomData<M>,
}

impl<'a, M> TransactionPtrBatch<'a, M> {
    const TX_CORE_SIZE: usize = std::mem::size_of::<SharableTransactionRegion>();
    const TX_TOTAL_SIZE: usize = Self::TX_CORE_SIZE + std::mem::size_of::<M>();
    #[allow(dead_code, reason = "Invariant assertion")]
    const TX_BATCH_SIZE_ASSERT: () =
        assert!(Self::TX_TOTAL_SIZE * MAX_TRANSACTIONS_PER_MESSAGE < 4096);
    const TX_BATCH_META_OFFSET: usize = Self::TX_CORE_SIZE * MAX_TRANSACTIONS_PER_MESSAGE;

    /// # Safety
    /// - [`SharableTransactionBatchRegion`] must reference a valid offset and length
    ///   within the `allocator`.
    /// - ALL [`SharableTransactionRegion`]  within the batch must be valid.
    ///   See [`TransactionPtr::from_sharable_transaction_region`] for details.
    /// - `M` must match the actual `M` used within this allocation.
    pub unsafe fn from_sharable_transaction_batch_region(
        sharable_transaction_batch_region: &SharableTransactionBatchRegion,
        allocator: &'a Allocator,
    ) -> Self {
        let base = allocator.ptr_from_offset(sharable_transaction_batch_region.transactions_offset);
        let tx_ptr = base.cast();
        // SAFETY:
        // - Assuming the batch was originally allocated to support `M`, this call will also be
        //   safe.
        let meta_ptr = unsafe { base.byte_add(Self::TX_BATCH_META_OFFSET).cast() };

        Self {
            tx_ptr,
            meta_ptr,
            num_transactions: usize::from(sharable_transaction_batch_region.num_transactions),
            allocator,

            _meta: PhantomData,
        }
    }

    /// The number of transactions in this batch.
    pub const fn len(&self) -> usize {
        self.num_transactions
    }

    /// Whether the batch is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterator returning [`TransactionPtr`] for each transaction in the batch.
    pub fn iter(&'a self) -> impl Iterator<Item = (TransactionPtr, M)> + 'a {
        (0..self.num_transactions).map(|idx| unsafe {
            let tx = self.tx_ptr.add(idx);
            let tx = TransactionPtr::from_sharable_transaction_region(tx.as_ref(), self.allocator);
            let meta = self.meta_ptr.add(idx).read();

            (tx, meta)
        })
    }

    /// Free the transaction batch container.
    ///
    /// # Safety
    ///
    /// - [`SharableTransactionBatchRegion`] must be exclusively owned by this pointer.
    ///
    /// # Note
    ///
    /// This will not free the underlying transactions as their lifetimes may be differ from that of
    /// the batch.
    pub unsafe fn free(self) {
        unsafe { self.allocator.free(self.tx_ptr.cast()) }
    }
}
