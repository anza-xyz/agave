use {
    agave_scheduler_bindings::{
        MAX_TRANSACTIONS_PER_MESSAGE, SharableTransactionBatchRegion, SharableTransactionRegion,
    },
    core::ptr::NonNull,
    rts_alloc::Allocator,
    std::marker::PhantomData,
};

#[derive(Debug)]
pub struct TransactionPtr {
    ptr: NonNull<u8>,
    count: usize,
}

impl AsRef<[u8]> for TransactionPtr {
    fn as_ref(&self) -> &[u8] {
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
        // SAFETY: `sharable_transaction_region.offset` was allocated by `allocator`.
        let ptr = unsafe { allocator.ptr_from_offset(sharable_transaction_region.offset) };
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
///
/// `CAPACITY` determines the fixed position of the metadata array within the backing allocation.
/// Metadata readers and writers must use the same `CAPACITY`.
pub struct TransactionPtrBatch<'a, M = (), const CAPACITY: usize = MAX_TRANSACTIONS_PER_MESSAGE> {
    tx_ptr: NonNull<SharableTransactionRegion>,
    meta_ptr: NonNull<M>,
    num_transactions: usize,
    allocator: &'a Allocator,

    _meta: PhantomData<M>,
}

struct BatchLayout {
    core_end: usize,
    meta_start: usize,
    meta_end: usize,
}

impl<'a, M, const CAPACITY: usize> TransactionPtrBatch<'a, M, CAPACITY> {
    pub const TRANSACTION_CORE_SIZE: usize = size_of::<SharableTransactionRegion>();

    /// Evaluating the layout validates `CAPACITY` and `M`.
    const LAYOUT: BatchLayout = {
        assert!(CAPACITY <= MAX_TRANSACTIONS_PER_MESSAGE);
        let core_end = Self::TRANSACTION_CORE_SIZE * CAPACITY;
        let meta_start = core_end.next_multiple_of(align_of::<M>());
        let meta_end = meta_start + size_of::<M>() * CAPACITY;
        assert!(meta_end <= 4096);
        BatchLayout {
            core_end,
            meta_start,
            meta_end,
        }
    };

    pub const TRANSACTION_CORE_END: usize = Self::LAYOUT.core_end;
    pub const TRANSACTION_META_START: usize = Self::LAYOUT.meta_start;
    pub const TRANSACTION_META_SIZE: usize = size_of::<M>() * CAPACITY;
    pub const TRANSACTION_META_END: usize = Self::LAYOUT.meta_end;

    /// Allocates a batch container for up to `CAPACITY` transaction regions and metadata values.
    pub fn allocate(allocator: &'a Allocator) -> Option<Self> {
        let allocation = allocator.allocate(Self::TRANSACTION_META_END as u32)?;
        let base = allocation;
        let tx_ptr = base.cast();
        // SAFETY: `Self::TRANSACTION_META_START` is within the allocation made above.
        let meta_ptr = unsafe { base.byte_add(Self::TRANSACTION_META_START).cast() };

        Some(Self {
            tx_ptr,
            meta_ptr,
            num_transactions: 0,
            allocator,

            _meta: PhantomData,
        })
    }

    /// # Safety
    /// - [`SharableTransactionBatchRegion`] must reference a valid offset and length
    ///   within the `allocator`.
    /// - ALL [`SharableTransactionRegion`]  within the batch must be valid.
    ///   See [`TransactionPtr::from_sharable_transaction_region`] for details.
    /// - `M` must match the actual `M` used within this allocation.
    /// - `CAPACITY` must match the capacity used when writing the metadata array.
    pub unsafe fn from_sharable_transaction_batch_region(
        sharable_transaction_batch_region: &SharableTransactionBatchRegion,
        allocator: &'a Allocator,
    ) -> Self {
        let num_transactions = usize::from(sharable_transaction_batch_region.num_transactions);
        assert!(
            num_transactions <= CAPACITY,
            "batch exceeds TransactionPtrBatch capacity"
        );
        // SAFETY: `sharable_transaction_batch_region.transactions_offset` was allocated by `allocator`.
        let base = unsafe {
            allocator.ptr_from_offset(sharable_transaction_batch_region.transactions_offset)
        };
        let tx_ptr = base.cast();
        // SAFETY:
        // - Assuming the batch was originally allocated to support `M`, this call will also be
        //   safe.
        let meta_ptr = unsafe { base.byte_add(Self::TRANSACTION_META_START).cast() };

        Self {
            tx_ptr,
            meta_ptr,
            num_transactions,
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

    /// Appends one transaction region and its associated metadata to the batch.
    ///
    /// Returns `Err` with the inputs unchanged when the batch is full.
    ///
    /// # Safety
    /// - `transaction` must reference valid, initialized bytes within this batch's allocator.
    /// - Those bytes must remain valid while the batch or its transaction pointers are used.
    pub unsafe fn try_push(
        &mut self,
        transaction: SharableTransactionRegion,
        meta: M,
    ) -> Result<(), (SharableTransactionRegion, M)>
    where
        M: Copy,
    {
        if self.num_transactions == CAPACITY {
            return Err((transaction, meta));
        }
        // SAFETY: `num_transactions` is strictly below this batch's capacity.
        unsafe {
            self.tx_ptr.add(self.num_transactions).write(transaction);
            self.meta_ptr.add(self.num_transactions).write(meta);
        }
        self.num_transactions = self.num_transactions.wrapping_add(1);
        Ok(())
    }

    /// Returns a transaction region that was previously written to `index`.
    pub fn transaction_region(&self, index: usize) -> SharableTransactionRegion {
        assert!(
            index < self.num_transactions,
            "batch index was not initialized"
        );
        // SAFETY: `index` was checked against the initialized transaction count above.
        unsafe { self.tx_ptr.add(index).read() }
    }

    /// Returns the sharable message region for this initialized batch.
    pub fn to_sharable_transaction_batch_region(&self) -> SharableTransactionBatchRegion {
        // SAFETY: `tx_ptr` was derived from this allocator when the batch was allocated.
        let transactions_offset = unsafe { self.allocator.offset(self.tx_ptr.cast()) };
        SharableTransactionBatchRegion {
            num_transactions: self
                .num_transactions
                .try_into()
                .expect("batch capacity is at most 64"),
            transactions_offset,
        }
    }

    /// Frees every transaction allocation referenced by this batch.
    ///
    /// This does not free the batch container; call [`Self::free`] afterwards when it is no
    /// longer needed.
    ///
    /// # Safety
    ///
    /// - This batch must be exclusively owned.
    /// - Every transaction region must reference a unique allocation owned by this allocator.
    /// - The batch must not be iterated over or sent after this call.
    pub unsafe fn free_transactions(&self) {
        for index in 0..self.num_transactions {
            let transaction = self.transaction_region(index);
            // SAFETY: the caller guarantees that this transaction allocation is owned by this
            // allocator and has not already been freed.
            unsafe { self.allocator.free_offset(transaction.offset) };
        }
    }

    /// Frees every transaction allocation and the batch container.
    ///
    /// # Safety
    ///
    /// - The batch and its transactions must be exclusively owned, with no outstanding users.
    /// - Every transaction region must reference a unique, live allocation owned by this allocator.
    pub unsafe fn free_with_transactions(self) {
        // SAFETY: the caller guarantees exclusive ownership of the batch and its allocations.
        unsafe { self.free_transactions() };
        // SAFETY: the caller exclusively owns the batch container.
        unsafe { self.free() };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator() -> Allocator {
        let file = tempfile::tempfile().unwrap();
        // SAFETY: this fresh file is initialized exactly once.
        unsafe { Allocator::create(&file, 4 * 1024 * 1024, 1, 65536) }.unwrap()
    }

    fn transaction(allocator: &Allocator) -> TransactionPtr {
        let ptr = allocator.allocate(1).unwrap();
        // SAFETY: the freshly allocated byte is writable.
        unsafe { ptr.write(0) };
        // SAFETY: the allocation contains one initialized byte.
        unsafe { TransactionPtr::from_raw_parts(ptr, 1) }
    }

    #[test]
    fn try_push_preserves_entries_and_returns_inputs_when_full() {
        let allocator = allocator();
        let transactions = [
            transaction(&allocator),
            transaction(&allocator),
            transaction(&allocator),
        ];
        // SAFETY: each transaction was allocated by this allocator.
        let regions = transactions
            .each_ref()
            .map(|transaction| unsafe { transaction.to_sharable_transaction_region(&allocator) });
        let mut batch = TransactionPtrBatch::<u64, 2>::allocate(&allocator).unwrap();

        // SAFETY: the region references an initialized allocation kept alive until cleanup below.
        unsafe { batch.try_push(regions[0], 10) }.unwrap();
        // SAFETY: the region references an initialized allocation kept alive until cleanup below.
        unsafe { batch.try_push(regions[1], 20) }.unwrap();
        assert_eq!(
            // SAFETY: the region references an initialized allocation kept alive until cleanup below.
            unsafe { batch.try_push(regions[2], 30) },
            Err((regions[2], 30))
        );
        assert_eq!(batch.len(), 2);
        for (index, (_, meta)) in batch.iter().enumerate() {
            assert_eq!(batch.transaction_region(index), regions[index]);
            assert_eq!(meta, [10, 20][index]);
        }
        let [_, _, rejected] = transactions;
        // SAFETY: the batch owns the first two unique allocations.
        unsafe { batch.free_with_transactions() };
        // SAFETY: the rejected transaction remains exclusively ours and belongs to this allocator.
        unsafe { rejected.free(&allocator) };
        assert_eq!(allocator.outstanding_allocation_bytes(), 0);
    }
}
