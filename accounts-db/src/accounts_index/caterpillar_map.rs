use std::{
    fmt::Debug,
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use modular_bitfield::{bitfield, prelude::*};
use static_assertions::const_assert_eq;

/// Represents a compact payload that can be embedded within a `CaterpillarCell`.
///
/// Parent trait is automatically provided by #[bitfield] annotated structs as long as they
/// fit in 64 bits. However CompactPayload is required to span at most 61 bits.
pub trait CompactPayload: Specifier<Bytes = u64, InOut = Self> {}

/// Container capable of embedding a compact payload `C` or holding a boxed expanded payload `E`.
pub struct CaterpillarCell<C: CompactPayload, E> {
    /// Raw value - either an embedded (small) payload or address of a boxed full value
    raw_atomic: AtomicU64,
    _phantom: PhantomData<(C, E)>,
}

impl<C, E> CaterpillarCell<C, E>
where
    E: From<C>,
    C: From<E> + CompactPayload,
{
    pub fn from_compact(compact: C) -> Self {
        Self::from_transumtion_cell(TransmutationCell::from_compact(compact))
    }

    pub fn from_expanded(expanded: E) -> Self {
        Self::from_transumtion_cell(TransmutationCell::from_expanded(expanded))
    }

    fn from_transumtion_cell(transmution_cell: TransmutationCell<C, E>) -> Self {
        Self {
            raw_atomic: AtomicU64::new(transmution_cell.raw()),
            _phantom: PhantomData,
        }
    }

    pub fn as_view(&self) -> CaterpillarCellView<C, E> {
        TransmutationCell::from_raw(self.raw_atomic.load(Ordering::Relaxed)).into_view()
    }

    pub fn turn_to_expanded(&self) -> CaterpillarCellView<C, E> {
        let mut current_raw = self.raw_atomic.load(Ordering::Acquire);
        loop {
            let current_holder = TransmutationCell::from_raw(current_raw);
            match current_holder.into_view() {
                CaterpillarCellView::Compact(compact) => {
                    let new_holder = TransmutationCell::from_expanded(E::from(compact));
                    match self.raw_atomic.compare_exchange(
                        current_raw,
                        new_holder.raw(),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => return new_holder.into_view(),
                        Err(different_raw) => {
                            // entry that we created and tried to store will not be used, so deallocate it
                            new_holder.deallocate_box_if_expanded();
                            // continue loop to re-check if the entry is still compact
                            current_raw = different_raw
                        }
                    }
                }
                e @ CaterpillarCellView::Expanded(_) => return e,
            }
        }
    }

    pub fn turn_to_compact(&mut self) -> CaterpillarCellView<C, E> {
        let current_raw = self.raw_atomic.load(Ordering::Acquire);
        let current_transmute = TransmutationCell::from_raw(current_raw);
        if current_transmute.is_compact() {
            return current_transmute.into_view();
        }
        let expanded = current_transmute
            .deallocate_box_if_expanded()
            .expect("entry should be of expanded kind");
        let new_transmute = TransmutationCell::from_compact(C::from(expanded));
        // We have exclusive (&mut) access to the entry, so no other code is reading or updating
        // the raw atomic. This means we can just store new value without conflict.
        self.raw_atomic
            .store(new_transmute.raw(), Ordering::Release);
        new_transmute.into_view()
    }
}

impl<C: CompactPayload, E> Drop for CaterpillarCell<C, E> {
    fn drop(&mut self) {
        let previous_raw = self.raw_atomic.swap(0, Ordering::AcqRel);
        if previous_raw != 0 {
            TransmutationCell::<C, E>::from_raw(previous_raw).deallocate_box_if_expanded();
        }
    }
}

#[derive(Debug)]
pub enum CaterpillarCellView<'a, C, E> {
    Compact(C),
    Expanded(&'a E),
}

#[bitfield(bits = 3)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, BitfieldSpecifier)]
pub struct CommonPayloadInfo {
    /// Whether the payload is compact in the entry itself (otherwise entry stores address to heap memory)
    pub is_compact: bool,
    pub reserved: B2,
}

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CommonInfoAndPayload {
    pub common_info: CommonPayloadInfo,
    pub payload_repr: B61,
}

#[repr(C)]
union TransmutationCell<C, E> {
    raw: u64,
    structured: CommonInfoAndPayload,
    _phantom: PhantomData<(C, E)>,
}

impl<C: Specifier<Bytes = u64, InOut = C>, E> TransmutationCell<C, E> {
    const ALIGNED_PTR_SHIFT: u64 = CommonPayloadInfo::BITS as u64;

    fn from_raw(raw: u64) -> Self {
        assert_eq!(size_of::<Self>(), size_of::<u64>());
        assert_ne!(
            raw, 0,
            "0 is not a valid value, i.e. neither it is compact nor holds a valid heap address"
        );
        Self { raw }
    }

    fn from_compact(compact: C) -> Self {
        assert_eq!(C::BITS + CommonPayloadInfo::BITS, <u64 as Specifier>::BITS);
        let embedded_payload = C::into_bytes(compact).unwrap();
        let structured = CommonInfoAndPayload::new()
            .with_common_info(CommonPayloadInfo::new().with_is_compact(true))
            .with_payload_repr(embedded_payload);
        const_assert_eq!(size_of::<CommonInfoAndPayload>(), size_of::<u64>());
        Self { structured }
    }

    fn from_expanded(expanded: E) -> Self {
        assert_eq!(align_of::<E>(), align_of::<u64>());
        let expanded_ptr = NonNull::new(Box::into_raw(Box::new(expanded))).unwrap();
        let shifted_address = Self::make_ptr_shifted_address(expanded_ptr);
        let structured = CommonInfoAndPayload::new()
            .with_common_info(CommonPayloadInfo::new().with_is_compact(false))
            .with_payload_repr(shifted_address);
        Self { structured }
    }

    fn raw(&self) -> u64 {
        // Getting full raw representation of the cell is always safe
        unsafe { self.raw }
    }

    fn is_compact(&self) -> bool {
        unsafe { self.structured.common_info().is_compact() }
    }

    fn into_view<'a>(self) -> CaterpillarCellView<'a, C, E> {
        unsafe {
            if self.is_compact() {
                CaterpillarCellView::Compact(C::from_bytes(self.structured.payload_repr()).unwrap())
            } else {
                let ptr = self.get_expanded_ptr();
                CaterpillarCellView::Expanded(ptr.as_ref().unwrap())
            }
        }
    }

    /// If entry is expanded, converts the saved heap address to the entry itself
    ///
    /// Note: this will deallocate the entry from the heap
    fn deallocate_box_if_expanded(self) -> Option<E> {
        (!self.is_compact()).then(|| unsafe {
            let ptr = self.get_expanded_mut_ptr();
            *Box::from_raw(ptr)
        })
    }

    /// Return a down-shifted (by 3 bits) memory address of a non-null `E` pointer
    ///
    /// `E` must be a 8-byte aligned struct
    fn make_ptr_shifted_address(ptr: NonNull<E>) -> u64 {
        let address = ptr.addr().get() as u64;
        assert_eq!(address & (1 << Self::ALIGNED_PTR_SHIFT - 1), 0);
        address >> Self::ALIGNED_PTR_SHIFT
    }

    unsafe fn get_expanded_ptr(&self) -> *const E {
        unsafe {
            let address = self.structured.payload_repr() << Self::ALIGNED_PTR_SHIFT;
            address as *const E
        }
    }

    unsafe fn get_expanded_mut_ptr(&self) -> *mut E {
        unsafe {
            let address = self.structured.payload_repr() << Self::ALIGNED_PTR_SHIFT;
            address as *mut E
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use super::*;

    #[derive(Debug)]
    pub struct TestExpandedPayload {
        pub ref_count: u32,
        pub slot_list: RwLock<Vec<TestCompactPayload>>,
    }

    impl From<TestCompactPayload> for TestExpandedPayload {
        fn from(compact: TestCompactPayload) -> Self {
            Self {
                ref_count: 1,
                slot_list: RwLock::new(vec![compact]),
            }
        }
    }

    #[bitfield(bits = 61)]
    #[repr(C)]
    #[derive(Clone, Debug, BitfieldSpecifier)]
    pub struct TestCompactPayload {
        /// Storage id, capped at 2^29-1
        pub file_id: B29,
        pub offset_reduced: B31,
        pub is_zero_lamports: bool,
    }

    impl From<TestExpandedPayload> for TestCompactPayload {
        fn from(expanded: TestExpandedPayload) -> Self {
            let slot_list = expanded.slot_list.into_inner().unwrap();
            Self::new()
                .with_file_id(slot_list[0].file_id())
                .with_offset_reduced(slot_list[0].offset_reduced())
        }
    }

    impl CompactPayload for TestCompactPayload {}

    type TestCaterpillarCell = CaterpillarCell<TestCompactPayload, TestExpandedPayload>;

    #[test]
    fn test_cell_turning() {
        let compact = TestCompactPayload::new()
            .with_file_id(4534)
            .with_offset_reduced(99)
            .with_is_zero_lamports(true);
        assert_eq!(
            TestCompactPayload::from_bytes(compact.clone().into_bytes()).file_id(),
            4534
        );

        let compact_cell = TestCaterpillarCell::from_compact(compact);
        let CaterpillarCellView::Compact(compact) = compact_cell.as_view() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(compact.file_id(), 4534);
        assert_eq!(compact.offset_reduced(), 99);

        let expanded = TestExpandedPayload {
            ref_count: 2,
            slot_list: RwLock::new(vec![compact]),
        };
        let expanded_cell = TestCaterpillarCell::from_expanded(expanded);
        let CaterpillarCellView::Expanded(expanded) = expanded_cell.as_view() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(expanded.ref_count, 2);
        assert_eq!(expanded.slot_list.read().unwrap()[0].file_id(), 4534);
        assert_eq!(expanded.slot_list.read().unwrap()[0].offset_reduced(), 99);

        let turning_cell = compact_cell;
        let CaterpillarCellView::Expanded(expanded) = turning_cell.turn_to_expanded() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(expanded.ref_count, 1);
        assert_eq!(expanded.slot_list.read().unwrap()[0].file_id(), 4534);
        assert_eq!(expanded.slot_list.read().unwrap()[0].offset_reduced(), 99);

        let mut mut_turning_cell = turning_cell;
        mut_turning_cell.turn_to_compact();
        let CaterpillarCellView::Compact(compact) = mut_turning_cell.as_view() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(compact.file_id(), 4534);
        assert_eq!(compact.offset_reduced(), 99);
    }

    #[test]
    fn test_concurrent_expand() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_THREADS: usize = 5;
        const INITIAL_FILE_ID: u32 = 1000;

        // Create an initial compact entry
        let initial_compact = TestCompactPayload::new()
            .with_file_id(INITIAL_FILE_ID)
            .with_offset_reduced(100)
            .with_is_zero_lamports(false);

        let cell = Arc::new(TestCaterpillarCell::from_compact(initial_compact));

        // Create barrier to synchronize thread starts
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        // Spawn N threads, each adding an entry with unique file_id
        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|thread_id| {
                let cell_clone = Arc::clone(&cell);
                let barrier_clone = Arc::clone(&barrier);

                thread::spawn(move || {
                    barrier_clone.wait(); // Synchronize start

                    // All threads race to call turn_to_expanded
                    let view = cell_clone.turn_to_expanded();

                    match view {
                        CaterpillarCellView::Expanded(expanded) => {
                            // Create new entry with unique file_id for this thread
                            let unique_file_id = INITIAL_FILE_ID + 1000 + (thread_id as u32);
                            // Add entry to slot_list under write lock
                            expanded.slot_list.write().unwrap().push(
                                TestCompactPayload::new()
                                    .with_file_id(unique_file_id)
                                    .with_offset_reduced(50)
                                    .with_is_zero_lamports(true),
                            );

                            unique_file_id // Return the file_id this thread added
                        }
                        _ => panic!("Expected expanded entry after turn_to_expanded"),
                    }
                })
            })
            .collect();

        // Wait for all threads to complete and collect their file_ids
        let added_file_ids: Vec<u32> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .chain([INITIAL_FILE_ID])
            .collect();

        let CaterpillarCellView::Expanded(expanded) = cell.as_view() else {
            panic!("Expected expanded entry at end");
        };
        let slot_list = expanded.slot_list.read().unwrap();

        // Should have original entry + N added by threads
        assert_eq!(slot_list.len(), NUM_THREADS + 1);

        // Verify each thread's entry was added
        for (thread_id, &expected_file_id) in added_file_ids.iter().enumerate() {
            let thread_entry = slot_list
                .iter()
                .find(|e| e.file_id() == expected_file_id)
                .unwrap_or_else(|| panic!("Missing entry for thread {}", thread_id));

            assert_eq!(
                thread_entry.offset_reduced(),
                50 * (1 + thread_id / NUM_THREADS) as u32
            );
            assert_eq!(thread_entry.is_zero_lamports(), thread_id < NUM_THREADS);
        }

        assert_eq!(expanded.ref_count, 1);
    }
}
