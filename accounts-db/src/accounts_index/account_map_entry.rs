use {
    super::{
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        AtomicRefCount, DiskIndexValue, IndexValue, RefCount, SlotList,
    },
    crate::{account_info::AccountInfo, is_zero_lamport::IsZeroLamport},
    solana_clock::Slot,
    std::{
        fmt::Debug,
        ops::Deref,
        sync::{
            atomic::{AtomicBool, Ordering},
            RwLock, RwLockReadGuard, RwLockWriteGuard,
        },
    },
};

/// one entry in the in-mem accounts index
/// Represents the value for an account key in the in-memory accounts index
#[derive(Debug)]
pub struct AccountMapEntry<T> {
    /// number of alive slots that contain >= 1 instances of account data for this pubkey
    /// where alive represents a slot that has not yet been removed by clean via AccountsDB::clean_stored_dead_slots() for containing no up to date account information
    ref_count: AtomicRefCount,
    /// list of slots in which this pubkey was updated
    /// Note that 'clean' removes outdated entries (ie. older roots) from this slot_list
    /// purge_slot() also removes non-rooted slots from this list
    slot_list: RwLock<SlotList<T>>,
    /// synchronization metadata for in-memory state since last flush to disk accounts index
    meta: AccountMapEntryMeta,
}

// Ensure the size of AccountMapEntry never changes unexpectedly
const _: () = assert!(size_of::<AccountMapEntry<AccountInfo>>() == 48);

impl<T: IndexValue> AccountMapEntry<T> {
    pub fn new(slot_list: SlotList<T>, ref_count: RefCount, meta: AccountMapEntryMeta) -> Self {
        Self {
            slot_list: RwLock::new(slot_list),
            ref_count: AtomicRefCount::new(ref_count),
            meta,
        }
    }

    #[cfg(test)]
    pub(super) fn empty_for_tests() -> Self {
        Self {
            slot_list: RwLock::default(),
            ref_count: AtomicRefCount::default(),
            meta: AccountMapEntryMeta::default(),
        }
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
    /// Do not call it while guard from any locking function (`slot_list_*lock`) is active.
    pub fn slot_list_lock_read_len(&self) -> usize {
        self.slot_list.read().unwrap().len()
    }

    /// Acquire a read lock on the slot list and return accessor for interpreting its representation
    ///
    /// Do not call any locking function (`slot_list_*lock*`) on the same `AccountMapEntry` until accessor
    /// they return is dropped.
    pub fn slot_list_read_lock(&self) -> SlotListReadGuard<'_, T> {
        SlotListReadGuard(self.slot_list.read().unwrap())
    }

    /// Acquire a write lock on the slot list and return accessor for modifying it
    ///
    /// Do not call any locking function (`slot_list_*lock*`) on the same `AccountMapEntry` until accessor
    /// they return is dropped.
    pub fn slot_list_write_lock(&self) -> SlotListWriteGuard<'_, T> {
        SlotListWriteGuard(self.slot_list.write().unwrap())
    }
}

/// Holds slot list lock for reading and provides read access to its contents.
#[derive(Debug)]
pub struct SlotListReadGuard<'a, T>(RwLockReadGuard<'a, SlotList<T>>);

impl<T> Deref for SlotListReadGuard<'_, T> {
    type Target = [(Slot, T)];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl<T> SlotListReadGuard<'_, T> {
    #[cfg(test)]
    pub fn clone_list(&self) -> SlotList<T>
    where
        T: Copy,
    {
        self.0.iter().copied().collect()
    }
}

/// Holds slot list lock for writing and provides mutable API translating changes to the slot list.
#[derive(Debug)]
pub struct SlotListWriteGuard<'a, T>(RwLockWriteGuard<'a, SlotList<T>>);

impl<T> SlotListWriteGuard<'_, T> {
    /// Append element to the end of slot list
    pub fn push(&mut self, item: (Slot, T)) {
        self.0.push(item);
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// Returns number of preserved elements (size of the slot list after processing).
    pub fn retain_and_count<F>(&mut self, f: F) -> usize
    where
        F: FnMut(&mut (Slot, T)) -> bool,
    {
        self.0.retain(f);
        self.0.len()
    }

    /// Clears the list, removing all elements.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.0.clear();
    }

    #[cfg(test)]
    pub fn assign(&mut self, value: impl IntoIterator<Item = (Slot, T)>) {
        *self.0 = value.into_iter().collect();
    }

    #[cfg(test)]
    pub fn clone_list(&self) -> SlotList<T>
    where
        T: Copy,
    {
        self.0.iter().copied().collect()
    }
}

impl<T> Deref for SlotListWriteGuard<'_, T> {
    type Target = [(Slot, T)];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
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
}

impl AccountMapEntryMeta {
    pub fn new_dirty<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &BucketMapHolder<T, U>,
        is_cached: bool,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(true),
            age: AtomicAge::new(storage.future_age_to_flush(is_cached)),
        }
    }
    pub fn new_clean<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &BucketMapHolder<T, U>,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(false),
            age: AtomicAge::new(storage.future_age_to_flush(false)),
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
