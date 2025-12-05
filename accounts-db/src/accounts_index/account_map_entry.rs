use {
    super::{
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        AtomicRefCount, DiskIndexValue, IndexValue, RefCount, SlotList,
    },
    crate::{account_info::AccountInfo, is_zero_lamport::IsZeroLamport},
    solana_clock::Slot,
    std::{
        fmt::Debug,
        mem::ManuallyDrop,
        ops::Deref,
        sync::{
            atomic::{AtomicBool, Ordering},
            RwLock, RwLockReadGuard, RwLockWriteGuard,
        },
    },
};

/// one entry in the in-mem accounts index
/// Represents the value for an account key in the in-memory accounts index
pub struct AccountMapEntry<T: Copy> {
    /// number of alive slots that contain >= 1 instances of account data for this pubkey
    /// where alive represents a slot that has not yet been removed by clean via AccountsDB::clean_stored_dead_slots() for containing no up to date account information
    ref_count: AtomicRefCount,
    /// list of slots in which this pubkey was updated
    /// Note that 'clean' removes outdated entries (ie. older roots) from this slot_list
    /// purge_slot() also removes non-rooted slots from this list
    slot_list: RwLock<SlotListRepr<T>>,
    /// synchronization metadata for in-memory state since last flush to disk accounts index
    meta: AccountMapEntryMeta,
}

// Ensure the size of AccountMapEntry never changes unexpectedly
const _: () = assert!(size_of::<AccountMapEntry<AccountInfo>>() == 48);

impl<T: IndexValue> AccountMapEntry<T> {
    pub fn new(slot_list: SlotList<T>, ref_count: RefCount, meta: AccountMapEntryMeta) -> Self {
        let (is_single, slot_list_repr) = SlotListRepr::from_list(slot_list);
        Self {
            slot_list: RwLock::new(slot_list_repr),
            ref_count: AtomicRefCount::new(ref_count),
            meta: AccountMapEntryMeta {
                is_single: AtomicBool::new(is_single),
                ..meta
            },
        }
    }

    #[cfg(test)]
    pub(super) fn empty_for_tests() -> Self {
        Self::new(SlotList::new(), 0, AccountMapEntryMeta::default())
    }

    pub fn ref_count(&self) -> RefCount {
        self.ref_count.load(Ordering::Acquire)
    }

    pub fn addref(&self) {
        let previous = self.ref_count.fetch_add(1, Ordering::Release);
        // ensure ref count does not overflow
        assert_ne!(previous, RefCount::MAX);
        self.set_dirty(true);
    }

    /// decrement the ref count by one
    /// return the refcount prior to subtracting 1
    /// 0 indicates an under refcounting error in the system.
    pub fn unref(&self) -> RefCount {
        self.unref_by_count(1)
    }

    /// decrement the ref count by the passed in amount
    /// return the refcount prior to the ref count change
    pub fn unref_by_count(&self, count: RefCount) -> RefCount {
        let previous = self.ref_count.fetch_sub(count, Ordering::Release);
        self.set_dirty(true);
        assert!(
            previous >= count,
            "decremented ref count below zero: {self:?}"
        );
        previous
    }

    pub fn dirty(&self) -> bool {
        self.meta.dirty.load(Ordering::Acquire)
    }

    pub fn set_dirty(&self, value: bool) {
        self.meta.dirty.store(value, Ordering::Release)
    }

    /// set dirty to false, return true if was dirty
    pub fn clear_dirty(&self) -> bool {
        self.meta
            .dirty
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    pub fn age(&self) -> Age {
        self.meta.age.load(Ordering::Acquire)
    }

    pub fn set_age(&self, value: Age) {
        self.meta.age.store(value, Ordering::Release)
    }

    /// set age to 'next_age' if 'self.age' is 'expected_age'
    pub fn try_exchange_age(&self, next_age: Age, expected_age: Age) {
        let _ = self.meta.age.compare_exchange(
            expected_age,
            next_age,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Return length of the slot list
    ///
    /// This function might need to acquire a read lock on the slot list, so it should not be called
    /// while any slot list accessor is active (since they hold the lock).
    pub fn slot_list_lock_read_len(&self) -> usize {
        if self.meta.is_single.load(Ordering::Acquire) {
            1 // single item
        } else {
            self.slot_list_read_lock().len()
        }
    }

    /// Acquire a read lock on the slot list and return accessor for interpreting its representation
    ///
    /// Do not call any locking function (`slot_list_*lock*`) on the same `AccountMapEntry` until accessor
    /// they return is dropped.
    pub fn slot_list_read_lock(&self) -> SlotListReadGuard<'_, T> {
        let repr_guard = self.slot_list.read().unwrap();
        SlotListReadGuard {
            repr_guard,
            is_single: self.meta.is_single.load(Ordering::Relaxed),
        }
    }

    /// Acquire a write lock on the slot list and return accessor for modifying it
    ///
    /// Do not call any locking function (`slot_list_*lock*`) on the same `AccountMapEntry` until accessor
    /// they return is dropped.
    pub fn slot_list_write_lock(&self) -> SlotListWriteGuard<'_, T> {
        SlotListWriteGuard {
            repr_guard: self.slot_list.write().unwrap(),
            meta: &self.meta,
        }
    }
}

impl<T: Copy + Debug> Debug for AccountMapEntry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slot_list_maybe_locked = self.slot_list.try_read().map(|rl| SlotListReadGuard {
            repr_guard: rl,
            is_single: self.meta.is_single.load(Ordering::Relaxed),
        });
        f.debug_struct("AccountMapEntry")
            .field("meta", &self.meta)
            .field("ref_count", &self.ref_count)
            .field("slot_list", &slot_list_maybe_locked)
            .finish()
    }
}

impl<T: Copy> Drop for AccountMapEntry<T> {
    fn drop(&mut self) {
        if !self.meta.is_single.load(Ordering::Acquire) {
            // Make drop panic-resistant
            if let Ok(mut slot_list) = self.slot_list.write() {
                // Safety: we operate on &mut self, so is_single==false won't change since above check
                unsafe { ManuallyDrop::drop(&mut slot_list.dynamic) }
            }
        }
    }
}

/// Slot list with dynamic number of elements
///
/// `None` indicates empty list such that no allocation is performed until elements are added
#[allow(clippy::box_collection)]
struct SlotListDynamic<T: Copy>(Option<Box<Vec<(Slot, T)>>>);

impl<T: Copy> SlotListDynamic<T> {
    const fn empty() -> Self {
        Self(None)
    }

    fn new(slot_list: Vec<(Slot, T)>) -> Self {
        if slot_list.is_empty() {
            Self::empty()
        } else {
            Self(Some(Box::new(slot_list)))
        }
    }
}

/// Representation of a slot list with denominator stored in entry metadata to minimize memory usage
union SlotListRepr<T: Copy> {
    /// This variant is used when entry's metadata `is_single` loads as `true` while holding the lock
    single: (Slot, T),
    /// Dynamically sized slot list (usually with size different than 1), used when `is_single` loads as `false`
    dynamic: ManuallyDrop<SlotListDynamic<T>>,
}

impl<T: Copy> SlotListRepr<T> {
    fn from_list(slot_list: SlotList<T>) -> (bool, Self) {
        if slot_list.len() == 1 {
            let single = slot_list[0];
            (true, Self { single })
        } else {
            let dynamic = ManuallyDrop::new(SlotListDynamic::new(slot_list.into_vec()));
            (false, Self { dynamic })
        }
    }

    // Safety: `is_single` needs to match current representation mode, thus this function is unsafe
    unsafe fn as_slice(&self, is_single: bool) -> &[(Slot, T)] {
        unsafe {
            if is_single {
                std::slice::from_ref(&self.single)
            } else {
                match self.dynamic.0.as_ref() {
                    Some(slot_list) => slot_list.as_slice(),
                    None => &[],
                }
            }
        }
    }
}

/// Holds slot list lock for reading and provides read access interpreting its representation.
pub struct SlotListReadGuard<'a, T: Copy> {
    repr_guard: RwLockReadGuard<'a, SlotListRepr<T>>,
    is_single: bool,
}

impl<T: Copy> Deref for SlotListReadGuard<'_, T> {
    type Target = [(Slot, T)];

    fn deref(&self) -> &Self::Target {
        unsafe { SlotListRepr::as_slice(&self.repr_guard, self.is_single) }
    }
}

impl<T: Copy> SlotListReadGuard<'_, T> {
    #[cfg(test)]
    pub fn clone_list(&self) -> SlotList<T>
    where
        T: Copy,
    {
        self.deref().iter().copied().collect()
    }
}

impl<T: Copy + Debug> Debug for SlotListReadGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}

/// Holds slot list lock for writing and provides mutable API translating changes to the representation.
///
/// Note: the adjustment of representation happens on-demand when transitioning from single to dynamic
/// and on `Drop` to check possible transition from dynamic to single.
pub struct SlotListWriteGuard<'a, T: Copy> {
    repr_guard: RwLockWriteGuard<'a, SlotListRepr<T>>,
    meta: &'a AccountMapEntryMeta,
}

impl<T: Copy> SlotListWriteGuard<'_, T> {
    /// Append element to the end of slot list
    pub fn push(&mut self, item: (Slot, T)) {
        if self.swap_is_single(false) {
            let existing_item = unsafe { self.repr_guard.single };
            self.repr_guard.dynamic =
                ManuallyDrop::new(SlotListDynamic::new(vec![existing_item, item]))
        } else {
            match unsafe { self.repr_guard.dynamic.0.as_mut() } {
                None => {
                    self.store_is_single(true);
                    self.repr_guard.single = item
                }
                Some(slot_list) => slot_list.push(item),
            }
        }
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// Returns number of preserved elements (size of the slot list after processing).
    pub fn retain_and_count<F>(&mut self, mut f: F) -> usize
    where
        F: FnMut(&mut (Slot, T)) -> bool,
    {
        if self.is_single() {
            let single_mut = unsafe { &mut self.repr_guard.single };
            if !f(single_mut) {
                self.store_is_single(false);
                // representation wasn't dynamic before, so no need to handle dropping existing value
                self.repr_guard.dynamic = ManuallyDrop::new(SlotListDynamic::empty());
                0
            } else {
                1
            }
        } else if let Some(slot_list) = unsafe { self.repr_guard.dynamic.0.as_mut() } {
            slot_list.retain_mut(f);
            slot_list.len()
        } else {
            0
        }
    }

    fn try_change_to_single(&mut self) {
        if !self.is_single() {
            if let Some(slot_list) = unsafe { self.repr_guard.dynamic.0.as_mut() } {
                if slot_list.len() == 1 {
                    let item = slot_list.pop().unwrap();
                    unsafe { ManuallyDrop::drop(&mut self.repr_guard.dynamic) };
                    self.repr_guard.single = item;
                    self.store_is_single(true);
                }
            }
        }
    }

    #[inline]
    fn is_single(&self) -> bool {
        // atomic access under write lock critical section doesn't require ordering
        self.meta.is_single.load(Ordering::Relaxed)
    }

    #[inline]
    fn swap_is_single(&self, new_val: bool) -> bool {
        // atomic access under write lock critical section doesn't require ordering
        self.meta.is_single.swap(new_val, Ordering::Relaxed)
    }

    #[inline]
    fn store_is_single(&self, new_val: bool) {
        // atomic access under write lock critical section doesn't require ordering
        self.meta.is_single.store(new_val, Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.assign(vec![]);
    }

    #[cfg(test)]
    pub fn assign(&mut self, value: impl IntoIterator<Item = (Slot, T)>) {
        let (is_single, repr) = SlotListRepr::from_list(value.into_iter().collect());
        // Representation is going to be replaced, so drop any existing dynamic value
        if !self.swap_is_single(is_single) {
            // This will deallocate any box(vec) in dynamic slot list
            unsafe { ManuallyDrop::drop(&mut self.repr_guard.dynamic) };
        }
        *self.repr_guard = repr;
    }

    #[cfg(test)]
    pub fn clone_list(&self) -> SlotList<T>
    where
        T: Copy,
    {
        self.deref().iter().copied().collect()
    }
}

impl<T: Copy> Drop for SlotListWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.try_change_to_single();
    }
}

impl<T: Copy> Deref for SlotListWriteGuard<'_, T> {
    type Target = [(Slot, T)];

    fn deref(&self) -> &Self::Target {
        let is_single = self.meta.is_single.load(Ordering::Acquire);
        unsafe { SlotListRepr::as_slice(&self.repr_guard, is_single) }
    }
}

impl<T: Copy + Debug> Debug for SlotListWriteGuard<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}

/// data per entry in in-mem accounts index
/// used to keep track of consistency with disk index
#[derive(Debug, Default)]
pub struct AccountMapEntryMeta {
    /// true if entry in in-mem idx has changes and needs to be written to disk
    dirty: AtomicBool,
    /// 'age' at which this entry should be purged from the cache (implements lru)
    age: AtomicAge,
    /// Marker for intepreting `SlotListRepr` as either a single item or dynamic list.
    ///
    /// Updated the size of the slot list changes between 1 and != 1 (changes to true may
    /// be delayed to avoid dropping already allocated dynamic list, e.g. until write lock
    /// to the slot list is released).
    is_single: AtomicBool,
}

impl AccountMapEntryMeta {
    pub fn new_dirty<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &BucketMapHolder<T, U>,
        is_cached: bool,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(true),
            age: AtomicAge::new(storage.future_age_to_flush(is_cached)),
            is_single: AtomicBool::default(), // overwritten when passed to create entry
        }
    }
    pub fn new_clean<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &BucketMapHolder<T, U>,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(false),
            age: AtomicAge::new(storage.future_age_to_flush(false)),
            is_single: AtomicBool::default(), // overwritten when passed to create entry
        }
    }
}

/// can be used to pre-allocate structures for insertion into accounts index outside of lock
pub enum PreAllocatedAccountMapEntry<T: IndexValue> {
    Entry(Box<AccountMapEntry<T>>),
    Raw((Slot, T)),
}

impl<T: IndexValue> IsZeroLamport for PreAllocatedAccountMapEntry<T> {
    fn is_zero_lamport(&self) -> bool {
        match self {
            PreAllocatedAccountMapEntry::Entry(entry) => {
                entry.slot_list_read_lock()[0].1.is_zero_lamport()
            }
            PreAllocatedAccountMapEntry::Raw(raw) => raw.1.is_zero_lamport(),
        }
    }
}

impl<T: IndexValue> From<PreAllocatedAccountMapEntry<T>> for (Slot, T) {
    fn from(source: PreAllocatedAccountMapEntry<T>) -> (Slot, T) {
        match source {
            PreAllocatedAccountMapEntry::Entry(entry) => entry.slot_list_read_lock()[0],
            PreAllocatedAccountMapEntry::Raw(raw) => raw,
        }
    }
}

impl<T: IndexValue> PreAllocatedAccountMapEntry<T> {
    /// create an entry that is equivalent to this process:
    /// 1. new empty (refcount=0, slot_list={})
    /// 2. update(slot, account_info)
    ///
    /// This code is called when the first entry [ie. (slot,account_info)] for a pubkey is inserted into the index.
    pub fn new<U: DiskIndexValue + From<T> + Into<T>>(
        slot: Slot,
        account_info: T,
        storage: &BucketMapHolder<T, U>,
        store_raw: bool,
    ) -> PreAllocatedAccountMapEntry<T> {
        if store_raw {
            Self::Raw((slot, account_info))
        } else {
            Self::Entry(Self::allocate(slot, account_info, storage))
        }
    }

    fn allocate<U: DiskIndexValue + From<T> + Into<T>>(
        slot: Slot,
        account_info: T,
        storage: &BucketMapHolder<T, U>,
    ) -> Box<AccountMapEntry<T>> {
        let is_cached = account_info.is_cached();
        let ref_count = RefCount::from(!is_cached);
        let meta = AccountMapEntryMeta::new_dirty(storage, is_cached);
        Box::new(AccountMapEntry::new(
            SlotList::from([(slot, account_info)]),
            ref_count,
            meta,
        ))
    }

    pub fn into_account_map_entry<U: DiskIndexValue + From<T> + Into<T>>(
        self,
        storage: &BucketMapHolder<T, U>,
    ) -> Box<AccountMapEntry<T>> {
        match self {
            Self::Entry(entry) => entry,
            Self::Raw((slot, account_info)) => Self::allocate(slot, account_info, storage),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        ahash::HashSet,
        std::{
            sync::{Arc, Barrier},
            thread,
        },
    };

    #[test]
    fn test_slot_list_write_guard_push() {
        let entry = AccountMapEntry::empty_for_tests();

        // Empty
        assert!(!entry.meta.is_single.load(Ordering::Acquire));
        assert_eq!(entry.slot_list_lock_read_len(), 0);

        // Push first element - should become single
        {
            let mut write_guard = entry.slot_list_write_lock();
            write_guard.push((10, 100));
            assert_eq!(write_guard.len(), 1);
            assert_eq!(write_guard[0], (10, 100));
        }
        assert!(entry.meta.is_single.load(Ordering::Acquire));
        assert_eq!(entry.slot_list_lock_read_len(), 1);

        // Push second element - should become dynamic
        {
            let mut write_guard = entry.slot_list_write_lock();
            write_guard.push((20, 200));
            assert_eq!(write_guard.len(), 2);
            assert_eq!(write_guard[0], (10, 100));
            assert_eq!(write_guard[1], (20, 200));
        }
        assert!(!entry.meta.is_single.load(Ordering::Acquire));
        assert_eq!(entry.slot_list_lock_read_len(), 2);
    }

    #[test]
    fn test_slot_list_write_guard_retain_and_count() {
        const FULL_LIST: [(Slot, u64); 4] = [(10, 1), (20, 2), (30, 3), (40, 4)];

        for i in 0..FULL_LIST.len() {
            let entry = AccountMapEntry::empty_for_tests();
            for item in &FULL_LIST[..i] {
                entry.slot_list_write_lock().push(*item);
            }
            assert_eq!(entry.slot_list_lock_read_len(), i);

            // Retain only even values
            let mut write_guard = entry.slot_list_write_lock();
            let count = write_guard.retain_and_count(|(_slot, info)| *info % 2 == 0);

            assert_eq!(count, i / 2);
            assert_eq!(write_guard.len(), i / 2);
        }
    }

    #[test]
    fn test_writer_serialization() {
        let entry = Arc::new(AccountMapEntry::empty_for_tests());
        let num_writers = 5;
        let barrier = Arc::new(Barrier::new(num_writers));

        let handles: Vec<_> = (0u64..num_writers as u64)
            .map(|i| {
                let entry = Arc::clone(&entry);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();

                    // Each writer will add multiple elements
                    for j in 0u64..3 {
                        let mut write_guard = entry.slot_list_write_lock();
                        let slot = (i * 10) + j;
                        let info = (i * 100) + j;
                        write_guard.push((slot, info));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("Thread should not panic");
        }

        // Verify all elements were added correctly
        let read_guard = entry.slot_list_read_lock();
        assert_eq!(read_guard.len(), num_writers * 3);

        // Since writers are serialized, elements should be added in some order
        // We can't guarantee the exact order due to thread scheduling, but we can
        // verify that all expected elements are present
        let mut found_elements = HashSet::default();
        for &(slot, info) in read_guard.iter() {
            found_elements.insert((slot, info));
        }

        for i in 0u64..num_writers as u64 {
            for j in 0u64..3 {
                let expected_slot = (i * 10) + j;
                let expected_info = (i * 100) + j;
                assert!(found_elements.contains(&(expected_slot, expected_info)));
            }
        }
    }
}
