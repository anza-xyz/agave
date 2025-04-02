use {
    super::{AtomicRefCount, DiskIndexValue, IndexValue, RefCount, SlotList},
    crate::{
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        is_zero_lamport::IsZeroLamport,
    },
    log::*,
    solana_clock::Slot,
    std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

/// one entry in the in-mem accounts index
/// Represents the value for an account key in the in-memory accounts index
#[derive(Debug, Default)]
pub struct AccountMapEntry<T> {
    /// number of alive slots that contain >= 1 instances of account data for this pubkey
    /// where alive represents a slot that has not yet been removed by clean via AccountsDB::clean_stored_dead_slots() for containing no up to date account information
    ref_count: AtomicRefCount,
    /// list of slots in which this pubkey was updated
    /// Note that 'clean' removes outdated entries (ie. older roots) from this slot_list
    /// purge_slot() also removes non-rooted slots from this list
    pub slot_list: RwLock<SlotList<T>>,
    /// synchronization metadata for in-memory state since last flush to disk accounts index
    pub meta: AccountMapEntryMeta,
}

impl<T: IndexValue> AccountMapEntry<T> {
    pub fn new(slot_list: SlotList<T>, ref_count: RefCount, meta: AccountMapEntryMeta) -> Self {
        Self {
            slot_list: RwLock::new(slot_list),
            ref_count: AtomicRefCount::new(ref_count),
            meta,
        }
    }
    pub fn ref_count(&self) -> RefCount {
        self.ref_count.load(Ordering::Acquire)
    }

    pub fn addref(&self) {
        self.ref_count.fetch_add(1, Ordering::Release);
        self.set_dirty(true);
    }

    /// decrement the ref count
    /// return the refcount prior to subtracting 1
    /// 0 indicates an under refcounting error in the system.
    pub fn unref(&self) -> RefCount {
        let previous = self.ref_count.fetch_sub(1, Ordering::Release);
        self.set_dirty(true);
        if previous == 0 {
            inc_new_counter_info!("accounts_index-deref_from_0", 1);
        }
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
}

/// data per entry in in-mem accounts index
/// used to keep track of consistency with disk index
#[derive(Debug, Default)]
pub struct AccountMapEntryMeta {
    /// true if entry in in-mem idx has changes and needs to be written to disk
    pub dirty: AtomicBool,
    /// 'age' at which this entry should be purged from the cache (implements lru)
    pub age: AtomicAge,
}

impl AccountMapEntryMeta {
    pub fn new_dirty<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &Arc<BucketMapHolder<T, U>>,
        is_cached: bool,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(true),
            age: AtomicAge::new(storage.future_age_to_flush(is_cached)),
        }
    }
    pub fn new_clean<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>(
        storage: &Arc<BucketMapHolder<T, U>>,
    ) -> Self {
        AccountMapEntryMeta {
            dirty: AtomicBool::new(false),
            age: AtomicAge::new(storage.future_age_to_flush(false)),
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
                entry.slot_list.read().unwrap()[0].1.is_zero_lamport()
            }
            PreAllocatedAccountMapEntry::Raw(raw) => raw.1.is_zero_lamport(),
        }
    }
}

impl<T: IndexValue> From<PreAllocatedAccountMapEntry<T>> for (Slot, T) {
    fn from(source: PreAllocatedAccountMapEntry<T>) -> (Slot, T) {
        match source {
            PreAllocatedAccountMapEntry::Entry(entry) => entry.slot_list.read().unwrap()[0],
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
        storage: &Arc<BucketMapHolder<T, U>>,
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
        storage: &Arc<BucketMapHolder<T, U>>,
    ) -> Arc<AccountMapEntry<T>> {
        let is_cached = account_info.is_cached();
        let ref_count = RefCount::from(!is_cached);
        let meta = AccountMapEntryMeta::new_dirty(storage, is_cached);
        Arc::new(AccountMapEntry::new(
            vec![(slot, account_info)],
            ref_count,
            meta,
        ))
    }

    pub fn into_account_map_entry<U: DiskIndexValue + From<T> + Into<T>>(
        self,
        storage: &Arc<BucketMapHolder<T, U>>,
    ) -> Arc<AccountMapEntry<T>> {
        match self {
            Self::Entry(entry) => entry,
            Self::Raw((slot, account_info)) => Self::allocate(slot, account_info, storage),
        }
    }
}
