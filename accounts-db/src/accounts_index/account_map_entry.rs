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
    pub meta: AccountMapEntryMeta,
}

impl<T: IndexValue> AccountMapEntry<T> {
    pub fn new(slot_list: SlotList<T>, ref_count: RefCount, meta: AccountMapEntryMeta) -> Self {
        Self {
            slot_list: RwLock::new(SlotListRepr {
                list: ManuallyDrop::new(Box::new(slot_list.to_vec())),
            }), // TODO: select repr
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

    pub fn slot_list_len(&self) -> usize {
        if !self.meta.is_regular.load(Ordering::Acquire) {
            let slot_list_repr = self.slot_list.read().unwrap();
            if !self.meta.is_regular.load(Ordering::Acquire) {
                // Safety: `is_regular` confirmed to be false while holding the lock
                return unsafe { slot_list_repr.list.len() };
            }
        }
        1 // regular entry
    }

    pub fn slot_list(&self) -> SLReadGuard<'_, T> {
        let repr_guard = self.slot_list.read().unwrap();
        SLReadGuard {
            repr_guard,
            is_regular: self.meta.is_regular.load(Ordering::Acquire),
        }
    }

    pub fn slot_list_mut(&self) -> SLWriteGuard<'_, T> {
        let repr_guard = self.slot_list.write().unwrap();
        SLWriteGuard {
            repr_guard,
            is_regular: self.meta.is_regular.load(Ordering::Acquire),
        }
    }
}

impl<T: IndexValue> Debug for AccountMapEntry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountMapEntry")
            .field("meta", &self.meta)
            .field("ref_count", &self.ref_count)
            .field("slot_list", &&*self.slot_list())
            .finish()
    }
}

impl<T: Copy> Drop for AccountMapEntry<T> {
    fn drop(&mut self) {
        if !self.meta.is_regular.load(Ordering::Acquire) {
            let mut slot_list = self.slot_list.write().unwrap();
            unsafe { ManuallyDrop::drop(&mut slot_list.list) }
        }
    }
}

union SlotListRepr<T: Copy> {
    /// This variant is used when entry's metadata `is_regular` loads as `true` while holding the lock
    singleton: (Slot, T),
    /// Slot list with potentially different number of elements than 1, used when `is_regular` loads as `false`
    list: ManuallyDrop<Box<Vec<(Slot, T)>>>,
}

pub struct SLReadGuard<'a, T: Copy> {
    repr_guard: RwLockReadGuard<'a, SlotListRepr<T>>,
    is_regular: bool,
}

impl<'a, T: Copy> Deref for SLReadGuard<'a, T> {
    type Target = [(Slot, T)];

    fn deref(&self) -> &Self::Target {
        unsafe {
            if self.is_regular {
                std::slice::from_ref(&self.repr_guard.singleton)
            } else {
                &self.repr_guard.list
            }
        }
    }
}

pub struct SLWriteGuard<'a, T: Copy> {
    repr_guard: RwLockWriteGuard<'a, SlotListRepr<T>>,
    is_regular: bool,
}

impl<'a, T: Copy> SLWriteGuard<'a, T> {
    pub fn push(&mut self, item: (Slot, T)) {
        if self.is_regular {
            self.is_regular = false;
            unsafe {
                let single = self.repr_guard.singleton;
                self.repr_guard.list = ManuallyDrop::new(Box::new(vec![single, item]));
            }
        }
        unsafe {
            self.repr_guard.list.push(item);
        }
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&mut (Slot, T)) -> bool,
    {
        // TODO
        if self.is_regular {
            self.is_regular = false;
            unsafe {
                let single = self.repr_guard.singleton;
                self.repr_guard.list = ManuallyDrop::new(Box::new(vec![single]));
            }
        }
        unsafe {
            self.repr_guard.list.retain_mut(f);
        }
    }

    pub fn remove(&mut self, index: usize) {
        // TODO
        if self.is_regular {
            self.is_regular = false;
            unsafe {
                let single = self.repr_guard.singleton;
                self.repr_guard.list = ManuallyDrop::new(Box::new(vec![single]));
            }
        }
        unsafe {
            self.repr_guard.list.remove(index);
        }
    }

    pub fn replace_at(&mut self, index: usize, item: (Slot, T)) -> (Slot, T) {
        if self.is_regular {
            self.is_regular = false;
            unsafe {
                let single = self.repr_guard.singleton;
                self.repr_guard.list = ManuallyDrop::new(Box::new(vec![single]));
            }
        }
        unsafe {
            self.repr_guard.list[index] = item;
        }
        item // TODO
    }
}

impl<'a, T: Copy> Deref for SLWriteGuard<'a, T> {
    type Target = [(Slot, T)];

    fn deref(&self) -> &Self::Target {
        unsafe {
            if self.is_regular {
                std::slice::from_ref(&self.repr_guard.singleton)
            } else {
                &self.repr_guard.list
            }
        }
    }
}

/// data per entry in in-mem accounts index
/// used to keep track of consistency with disk index
#[derive(Debug, Default)]
pub struct AccountMapEntryMeta {
    /// true if entry in in-mem idx has changes and needs to be written to disk
    pub dirty: AtomicBool,
    /// 'age' at which this entry should be purged from the cache (implements lru)
    pub age: AtomicAge,
    /// Marker for intepreting `SlotListRepr` as either a singleton or a list.
    ///
    /// It is updated when write access to the slot list is released and the entry kind
    /// (regular vs irregular) changes.
    pub is_regular: AtomicBool,
}

impl AccountMapEntryMeta {
    pub fn new_dirty<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &BucketMapHolder<T, U>,
        is_cached: bool,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(true),
            age: AtomicAge::new(storage.future_age_to_flush(is_cached)),
            is_regular: AtomicBool::new(false),
        }
    }
    pub fn new_clean<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &BucketMapHolder<T, U>,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(false),
            age: AtomicAge::new(storage.future_age_to_flush(false)),
            is_regular: AtomicBool::new(true),
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
            PreAllocatedAccountMapEntry::Entry(entry) => entry.slot_list()[0].1.is_zero_lamport(),
            PreAllocatedAccountMapEntry::Raw(raw) => raw.1.is_zero_lamport(),
        }
    }
}

impl<T: IndexValue> From<PreAllocatedAccountMapEntry<T>> for (Slot, T) {
    fn from(source: PreAllocatedAccountMapEntry<T>) -> (Slot, T) {
        match source {
            PreAllocatedAccountMapEntry::Entry(entry) => entry.slot_list()[0],
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
