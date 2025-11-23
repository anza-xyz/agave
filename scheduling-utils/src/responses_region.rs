use {
    agave_scheduler_bindings::{
        worker_message_types::{
            self, CheckResponse, ExecutionResponse, CHECK_RESPONSE, EXECUTION_RESPONSE,
        },
        TransactionResponseRegion,
    },
    rts_alloc::Allocator,
    std::ptr::NonNull,
};

/// Prepare a [`TransactionResponseRegion`] with [`ExecutionResponse`].
pub fn execution_responses_from_iter(
    allocator: &Allocator,
    iter: impl ExactSizeIterator<Item = ExecutionResponse>,
) -> Option<TransactionResponseRegion> {
    // SAFETY: EXECUTION_RESPONSE -> ExecutionResponse
    unsafe { from_iterator(allocator, EXECUTION_RESPONSE, iter) }
}

/// Prepare a [`TransactionResponseRegion`] with [`CheckResponse`].
pub fn resolve_responses_from_iter(
    allocator: &Allocator,
    iter: impl ExactSizeIterator<Item = CheckResponse>,
) -> Option<TransactionResponseRegion> {
    // SAFETY: CHECK_RESPONSE -> CheckResponse
    unsafe { from_iterator(allocator, CHECK_RESPONSE, iter) }
}

/// Allocate region a [`TransactionResponseRegion`] with [`CheckResponse`].
/// Each [`CheckResponse`] is not yet populated and must be populated by the
/// caller.
pub fn allocate_check_response_region(
    allocator: &Allocator,
    num_transaction_responses: usize,
) -> Option<(NonNull<CheckResponse>, TransactionResponseRegion)> {
    // SAFETY: CHECK_RESPONSE -> CheckResponse
    unsafe {
        allocate_response_region::<CheckResponse>(
            allocator,
            CHECK_RESPONSE,
            num_transaction_responses,
        )
    }
}

/// Allocate a response region.
///
/// # Safety
/// - T must be a valid response type
/// - `tag` must match the `T`
unsafe fn allocate_response_region<T: Sized>(
    allocator: &Allocator,
    tag: u8,
    num_transaction_responses: usize,
) -> Option<(NonNull<T>, TransactionResponseRegion)> {
    let size = num_transaction_responses.wrapping_mul(core::mem::size_of::<T>());
    let response_ptr = allocator.allocate(size as u32)?.cast::<T>();
    debug_assert!(
        response_ptr.is_aligned(),
        "allocator should guarantee alignment for the response types of interest"
    );

    // SAFETY: `response_ptr` was allocated from the allocator.
    let transaction_responses_offset = unsafe { allocator.offset(response_ptr.cast()) };

    Some((
        response_ptr,
        TransactionResponseRegion {
            tag,
            num_transaction_responses: num_transaction_responses as u8,
            transaction_responses_offset,
        },
    ))
}

/// Prepare a [`TransactionResponseRegion`] from an iterator.
///
/// # Safety
/// - T must be a valid response type
/// - `tag` must match the `T`
unsafe fn from_iterator<T: Sized>(
    allocator: &Allocator,
    tag: u8,
    iter: impl ExactSizeIterator<Item = T>,
) -> Option<TransactionResponseRegion> {
    let num_transaction_responses = iter.len();
    let (response_ptr, region) =
        unsafe { allocate_response_region(allocator, tag, num_transaction_responses)? };
    for (index, response) in iter.enumerate() {
        // SAFETY: `response_ptr` is sufficiently sized to fit the response vector.
        unsafe { response_ptr.add(index).write(response) };
    }

    Some(region)
}

#[derive(Debug)]
pub struct CheckResponsesPtr {
    ptr: NonNull<CheckResponse>,
    count: usize,
}

impl CheckResponsesPtr {
    /// Constructions a [`CheckResponsesPtr`] from raw parts.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid for reads.
    /// - `count` must be accurate (in number of responses) and not overrun the end of `ptr`.
    ///
    /// # Note
    ///
    /// If you are trying to construct a pointer for use by Agave, you almost certainly want to use
    /// [`Self::from_transaction_response_region`].
    #[cfg(feature = "dev-context-only-utils")]
    pub unsafe fn from_raw_parts(ptr: NonNull<CheckResponse>, count: usize) -> Self {
        Self { ptr, count }
    }

    /// Constructs the pointer from a [`TransactionResponseRegion`].
    ///
    /// # Safety
    ///
    /// - The provided [`TransactionResponseRegion`] must be of type
    ///   [`worker_message_types::CHECK_RESPONSE`].
    /// - The allocation pointed to by this region must not have previously been freed.
    pub unsafe fn from_transaction_response_region(
        transaction_response_region: &TransactionResponseRegion,
        allocator: &Allocator,
    ) -> Self {
        debug_assert!(transaction_response_region.tag == worker_message_types::CHECK_RESPONSE);

        Self {
            ptr: allocator
                .ptr_from_offset(transaction_response_region.transaction_responses_offset)
                .cast(),
            count: transaction_response_region.num_transaction_responses as usize,
        }
    }

    /// The number of responses in this batch.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the batch is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate the responses within the batch.
    pub fn iter(&self) -> impl Iterator<Item = &CheckResponse> {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }.iter()
    }

    /// Free the batch's allocation.
    ///
    /// # Safety
    ///
    /// - `Self` must be exclusively owned.
    pub unsafe fn free(self, allocator: &Allocator) {
        unsafe { allocator.free(self.ptr.cast()) }
    }
}

#[derive(Debug)]
pub struct ExecutionResponsesPtr {
    ptr: NonNull<ExecutionResponse>,
    count: usize,
}

impl ExecutionResponsesPtr {
    /// Constructions a [`ExecutionResponsesPtr`] from raw parts.
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid for reads.
    /// - `count` must be accurate (in number of responses) and not overrun the end of `ptr`.
    ///
    /// # Note
    ///
    /// If you are trying to construct a pointer for use by Agave, you almost certainly want to use
    /// [`Self::from_transaction_response_region`].
    #[cfg(feature = "dev-context-only-utils")]
    pub unsafe fn from_raw_parts(ptr: NonNull<ExecutionResponse>, count: usize) -> Self {
        Self { ptr, count }
    }

    /// Constructs the pointer from a [`TransactionResponseRegion`].
    ///
    /// # Safety
    ///
    /// - The provided [`TransactionResponseRegion`] must be of type
    ///   [`worker_message_types::EXECUTION_RESPONSE`].
    /// - The allocation pointed to by this region must not have previously been freed.
    pub unsafe fn from_transaction_response_region(
        transaction_response_region: &TransactionResponseRegion,
        allocator: &Allocator,
    ) -> Self {
        debug_assert!(transaction_response_region.tag == worker_message_types::EXECUTION_RESPONSE);

        Self {
            ptr: allocator
                .ptr_from_offset(transaction_response_region.transaction_responses_offset)
                .cast(),
            count: transaction_response_region.num_transaction_responses as usize,
        }
    }

    /// The number of responses in this batch.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the batch is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate the responses within the batch.
    pub fn iter(&self) -> impl Iterator<Item = &ExecutionResponse> {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.count) }.iter()
    }

    /// Free the batch's allocation.
    ///
    /// # Safety
    ///
    /// - `Self` must be exclusively owned.
    pub unsafe fn free(self, allocator: &Allocator) {
        unsafe { allocator.free(self.ptr.cast()) }
    }
}
