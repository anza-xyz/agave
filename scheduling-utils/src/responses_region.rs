use {
    agave_scheduler_bindings::{
        worker_message_types::{
            CheckResponse, ExecutionResponse, CHECK_RESPONSE, EXECUTION_RESPONSE,
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
        allocate_response_region(allocator, tag, num_transaction_responses)?;
    for (index, response) in iter.enumerate() {
        // SAFETY: `response_ptr` is sufficiently sized to fit the response vector.
        unsafe { response_ptr.add(index).write(response) };
    }

    Some(region)
}
