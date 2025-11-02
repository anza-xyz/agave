use {
    agave_scheduler_bindings::{
        worker_message_types::{ExecutionResponse, Resolved, EXECUTION_RESPONSE, RESOLVED},
        TransactionResponseRegion,
    },
    rts_alloc::Allocator,
};

/// Prepare a [`TransactionResponseRegion`] with [`ExecutionResponse`].
pub fn execution_responses_from_iter(
    allocator: &Allocator,
    iter: impl ExactSizeIterator<Item = ExecutionResponse>,
) -> Option<TransactionResponseRegion> {
    // SAFETY: EXECUTION_RESPONSE -> ExecutionResponse
    unsafe { from_iterator(allocator, EXECUTION_RESPONSE, iter) }
}

/// Prepare a [`TransactionResponseRegion`] with [`Resolved`].
pub fn resolve_responses_from_iter(
    allocator: &Allocator,
    iter: impl ExactSizeIterator<Item = Resolved>,
) -> Option<TransactionResponseRegion> {
    // SAFETY: RESOLVED -> Resolved
    unsafe { from_iterator(allocator, RESOLVED, iter) }
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
    let size = num_transaction_responses.wrapping_mul(core::mem::size_of::<T>());
    let response_ptr = allocator.allocate(size as u32)?.cast::<T>();

    for (index, response) in iter.enumerate() {
        debug_assert!(
            response_ptr.is_aligned(),
            "allocator should guarantee alignment for the response types of interest"
        );

        // SAFETY: `response_ptr` is sufficiently sized to fit the response vector.
        unsafe { response_ptr.add(index).write(response) };
    }

    // SAFETY: `response_ptr` was allocated from the allocator.
    let transaction_responses_offset = unsafe { allocator.offset(response_ptr.cast()) };

    Some(TransactionResponseRegion {
        tag,
        num_transaction_responses: num_transaction_responses as u8,
        transaction_responses_offset,
    })
}
