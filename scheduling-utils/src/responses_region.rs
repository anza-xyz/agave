use {
    agave_scheduler_bindings::{
        CheckResponseRegion, ExecutionResponseRegion,
        worker_message_types::{CheckResponse, ExecutionResponse},
    },
    rts_alloc::Allocator,
    std::ptr::NonNull,
};

/// Prepare an [`ExecutionResponseRegion`] with [`ExecutionResponse`].
pub fn execution_responses_from_iter(
    allocator: &Allocator,
    iter: impl ExactSizeIterator<Item = ExecutionResponse>,
) -> Option<ExecutionResponseRegion> {
    let num_transaction_responses = iter.len();
    let (response_ptr, transaction_responses_offset) =
        allocate_response_region(allocator, num_transaction_responses)?;
    write_responses(response_ptr, iter);

    Some(ExecutionResponseRegion {
        num_transaction_responses: num_transaction_responses as u8,
        transaction_responses_offset,
    })
}

/// Prepare a [`CheckResponseRegion`] with [`CheckResponse`].
pub fn resolve_responses_from_iter(
    allocator: &Allocator,
    iter: impl ExactSizeIterator<Item = CheckResponse>,
) -> Option<CheckResponseRegion> {
    let num_transaction_responses = iter.len();
    let (response_ptr, transaction_responses_offset) =
        allocate_response_region(allocator, num_transaction_responses)?;
    write_responses(response_ptr, iter);

    Some(CheckResponseRegion {
        num_transaction_responses: num_transaction_responses as u8,
        transaction_responses_offset,
    })
}

/// Allocate a [`CheckResponseRegion`] with [`CheckResponse`].
/// Each [`CheckResponse`] is not yet populated and must be populated by the
/// caller.
pub fn allocate_check_response_region(
    allocator: &Allocator,
    num_transaction_responses: usize,
) -> Option<(NonNull<CheckResponse>, CheckResponseRegion)> {
    let (response_ptr, transaction_responses_offset) =
        allocate_response_region(allocator, num_transaction_responses)?;

    Some((
        response_ptr,
        CheckResponseRegion {
            num_transaction_responses: num_transaction_responses as u8,
            transaction_responses_offset,
        },
    ))
}

/// Allocate a response region.
fn allocate_response_region<T: Sized>(
    allocator: &Allocator,
    num_transaction_responses: usize,
) -> Option<(NonNull<T>, usize)> {
    let size = num_transaction_responses.wrapping_mul(core::mem::size_of::<T>());
    let response_ptr = allocator.allocate(size as u32)?.cast::<T>();
    debug_assert!(
        response_ptr.is_aligned(),
        "allocator should guarantee alignment for the response types of interest"
    );

    // SAFETY: `response_ptr` was allocated from the allocator.
    let transaction_responses_offset = unsafe { allocator.offset(response_ptr.cast()) };

    Some((response_ptr, transaction_responses_offset))
}

fn write_responses<T: Sized>(response_ptr: NonNull<T>, iter: impl ExactSizeIterator<Item = T>) {
    for (index, response) in iter.enumerate() {
        // SAFETY: `response_ptr` is sufficiently sized to fit the response vector.
        unsafe { response_ptr.add(index).write(response) };
    }
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
    pub unsafe fn from_raw_parts(ptr: NonNull<CheckResponse>, count: usize) -> Self {
        Self { ptr, count }
    }

    /// Constructs the pointer from a [`CheckResponseRegion`].
    ///
    /// # Safety
    ///
    /// - The allocation pointed to by this region must be valid and not previously freed.
    pub unsafe fn from_transaction_response_region(
        transaction_response_region: &CheckResponseRegion,
        allocator: &Allocator,
    ) -> Self {
        Self {
            // SAFETY: `transaction_response_region.transaction_responses_offset` was allocated by `allocator`.
            ptr: unsafe {
                allocator.ptr_from_offset(transaction_response_region.transaction_responses_offset)
            }
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
    pub unsafe fn from_raw_parts(ptr: NonNull<ExecutionResponse>, count: usize) -> Self {
        Self { ptr, count }
    }

    /// Constructs the pointer from an [`ExecutionResponseRegion`].
    ///
    /// # Safety
    ///
    /// - The allocation pointed to by this region must be valid and not previously freed.
    pub unsafe fn from_transaction_response_region(
        transaction_response_region: &ExecutionResponseRegion,
        allocator: &Allocator,
    ) -> Self {
        Self {
            // SAFETY: `transaction_response_region.transaction_responses_offset` was allocated by `allocator`.
            ptr: unsafe {
                allocator.ptr_from_offset(transaction_response_region.transaction_responses_offset)
            }
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
