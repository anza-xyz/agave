use {
    super::{AtomicRefCount, DiskIndexValue, IndexValue, RefCount, SlotList},
    crate::{
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        is_zero_lamport::IsZeroLamport,
    },
    solana_clock::Slot,
    std::{
        fmt::Debug,
        mem::ManuallyDrop,
        ops::Deref,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock, RwLockReadGuard, RwLockWriteGuard,
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
        if !self.meta.is_single.load(Ordering::Acquire) {
            let slot_list_repr = self.slot_list.read().unwrap();
            if !self.meta.is_single.load(Ordering::Acquire) {
                // Safety: `is_single` confirmed to be false while holding the lock
                return unsafe { slot_list_repr.multiple.len() };
            }
        }
        1 // single list
    }

    /// Acquire a read lock on the slot list and return accessor for interpreting its representation
    ///
    /// Do not call any locking function (`slot_list_*lock*`) on the same `AccountMapEntry` until accessor
    /// they return is dropped.
    pub fn slot_list_read_lock(&self) -> SlotListReadGuard<'_, T> {
        let repr_guard = self.slot_list.read().unwrap();
        SlotListReadGuard {
            repr_guard,
            is_single: self.meta.is_single.load(Ordering::Acquire),
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

impl<T: Copy> Debug for AccountMapEntry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountMapEntry")
            .field("meta", &self.meta)
            .field("ref_count", &self.ref_count)
            .finish()
    }
}

impl<T: Copy> Drop for AccountMapEntry<T> {
    fn drop(&mut self) {
        if !self.meta.is_single.load(Ordering::Acquire) {
            // Make drop panic-resistant
            if let Ok(mut slot_list) = self.slot_list.write() {
                // Safety: we operate on &mut self, so is_single==false won't change since above check
                unsafe { ManuallyDrop::drop(&mut slot_list.multiple) }
            }
        }
    }
}

/// Dynamic representation of a slot list with denominator stored in entry metadata to minimize memory usage
union SlotListRepr<T: Copy> {
    /// This variant is used when entry's metadata `is_single` loads as `true` while holding the lock
    single: (Slot, T),
    /// Slot list with potentially different number of elements than 1, used when `is_single` loads as `false`
    // Vec holds data on heap, however its inline size is impacted by len and capacity fields, so it needs to
    // be wrapped in a Box to minimize the size of the union.
    #[allow(clippy::box_collection)]
    multiple: ManuallyDrop<Box<Vec<(Slot, T)>>>,
}

impl<T: Copy> SlotListRepr<T> {
    fn from_list(slot_list: SlotList<T>) -> (bool, Self) {
        let is_single = slot_list.len() == 1;
        let this = if is_single {
            Self {
                single: slot_list[0],
            }
        } else {
            Self {
                multiple: ManuallyDrop::new(Box::new(slot_list.to_vec())),
            }
        };
        (is_single, this)
    }

    // Safety: `is_single` needs to match current representation mode, thus this function is unsafe
    unsafe fn as_slice(&self, is_single: bool) -> &[(Slot, T)] {
        unsafe {
            if is_single {
                std::slice::from_ref(&self.single)
            } else {
                &self.multiple
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
/// Note: the adjustment of representation happens on-demand when transitioning from single to multiple
/// and on `Drop` to check possible transition from list to single.
pub struct SlotListWriteGuard<'a, T: Copy> {
    repr_guard: RwLockWriteGuard<'a, SlotListRepr<T>>,
    meta: &'a AccountMapEntryMeta,
}

impl<T: Copy> SlotListWriteGuard<'_, T> {
    /// Append element to the end of slot list
    pub fn push(&mut self, item: (Slot, T)) {
        self.change_to_multiple().push(item);
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// Returns number of preserved elements (size of the slot list after processing).
    pub fn retain_and_count<F>(&mut self, mut f: F) -> usize
    where
        F: FnMut(&mut (Slot, T)) -> bool,
    {
        if self.meta.is_single.load(Ordering::Acquire) {
            let single_mut = unsafe { &mut self.repr_guard.single };
            if !f(single_mut) {
                self.meta.is_single.store(false, Ordering::Release);
                self.repr_guard.multiple = ManuallyDrop::new(Box::new(vec![]));
                0
            } else {
                1
            }
        } else {
            let slot_list = unsafe { self.repr_guard.multiple.as_mut() };
            slot_list.retain_mut(f);
            slot_list.len()
        }
    }

    fn change_to_multiple(&mut self) -> &mut Vec<(Slot, T)> {
        if self.meta.is_single.swap(false, Ordering::AcqRel) {
            let single = unsafe { self.repr_guard.single };
            self.repr_guard.multiple = ManuallyDrop::new(Box::new(vec![single]));
        }
        unsafe { self.repr_guard.multiple.as_mut() }
    }

    fn try_change_to_single(&mut self) {
        if !self.meta.is_single.load(Ordering::Acquire) {
            let list = unsafe { &mut self.repr_guard.multiple };
            if list.len() == 1 {
                assert_eq!(list.len(), 1);
                let item = list.pop().unwrap();
                unsafe { ManuallyDrop::drop(list) };
                self.repr_guard.single = item;
                self.meta.is_single.store(true, Ordering::Release);
            }
        }
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.change_to_multiple().clear();
    }

    #[cfg(test)]
    pub fn assign(&mut self, value: impl IntoIterator<Item = (Slot, T)>) {
        *self.change_to_multiple() = value.into_iter().collect();
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
    /// Marker for intepreting `SlotListRepr` as either a single or a multiple.
    ///
    /// It is updated when write access to the slot list is released and the size of the slot
    /// list changes between 1 and != 1.
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
    Entry(Arc<AccountMapEntry<T>>),
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
    ) -> Arc<AccountMapEntry<T>> {
        let is_cached = account_info.is_cached();
        let ref_count = RefCount::from(!is_cached);
        let meta = AccountMapEntryMeta::new_dirty(storage, is_cached);
        Arc::new(AccountMapEntry::new(
            SlotList::from([(slot, account_info)]),
            ref_count,
            meta,
        ))
    }

    pub fn into_account_map_entry<U: DiskIndexValue + From<T> + Into<T>>(
        self,
        storage: &BucketMapHolder<T, U>,
    ) -> Arc<AccountMapEntry<T>> {
        match self {
            Self::Entry(entry) => entry,
            Self::Raw((slot, account_info)) => Self::allocate(slot, account_info, storage),
        }
    }
}
