use {
    super::{
        account_map_entry::{
            AccountMapEntry, AccountMapEntryMeta, PreAllocatedAccountMapEntry, SlotListWriteGuard,
        },
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        stats::Stats,
        DiskIndexValue, IndexValue, ReclaimsSlotList, RefCount, SlotList, SlotListItem,
        UpsertReclaim,
    },
    crate::pubkey_bins::PubkeyBinCalculator24,
    rand::{rng, Rng},
    solana_bucket_map::bucket_api::BucketApi,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    std::{
        cmp,
        collections::{hash_map::Entry, HashMap, HashSet},
        fmt::Debug,
        mem,
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex, RwLock,
        },
    },
};

#[derive(Debug, Default)]
pub struct StartupStats {
    pub copy_data_us: AtomicU64,
}

// one instance of this represents one bin of the accounts index.
pub struct InMemAccountsIndex<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    last_age_flushed: AtomicAge,

    // backing store
    map_internal: RwLock<HashMap<Pubkey, Box<AccountMapEntry<T>>, ahash::RandomState>>,
    storage: Arc<BucketMapHolder<T, U>>,
    _bin: usize,
    pub(crate) lowest_pubkey: Pubkey,
    pub(crate) highest_pubkey: Pubkey,

    bucket: Option<Arc<BucketApi<(Slot, U)>>>,

    // set to true while this bin is being actively flushed
    flushing_active: AtomicBool,

    /// info to streamline initial index generation
    startup_info: StartupInfo<T, U>,

    /// how many more ages to skip before this bucket is flushed (as opposed to being skipped).
    /// When this reaches 0, this bucket is flushed.
    remaining_ages_to_skip_flushing: AtomicAge,

    /// an individual bucket will evict its entries and write to disk every 1/NUM_AGES_TO_DISTRIBUTE_FLUSHES ages
    /// Higher numbers mean we flush less buckets/s
    /// Lower numbers mean we flush more buckets/s
    num_ages_to_distribute_flushes: Age,

    /// stats related to starting up
    pub(crate) startup_stats: Arc<StartupStats>,
}

impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> Debug for InMemAccountsIndex<T, U> {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// An entry was inserted into the index; did it already exist in the index?
#[derive(Debug)]
pub enum InsertNewEntryResults {
    DidNotExist,
    Existed {
        other_slot: Option<Slot>,
        location: ExistedLocation,
    },
}

/// An entry was inserted into the index that previously existed; where did it previously exist?
#[derive(Debug)]
pub enum ExistedLocation {
    InMem,
    OnDisk,
}

#[derive(Default, Debug)]
struct StartupInfoDuplicates<T: IndexValue> {
    /// entries that were found to have duplicate index entries.
    /// When all entries have been inserted, these can be resolved and held in memory.
    duplicates: Vec<(Slot, Pubkey, T)>,
    /// pubkeys that were already added to disk and later found to be duplicates,
    duplicates_put_on_disk: HashSet<(Slot, Pubkey)>,

    /// (slot, pubkey) pairs that are found to be duplicates when we are
    /// starting from in-memory only index. This field is used only when disk
    /// index is disabled.
    duplicates_from_in_memory_only: Vec<(Slot, Pubkey)>,
}

#[derive(Default, Debug)]
struct StartupInfo<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    /// entries to add next time we are flushing to disk
    insert: Mutex<Vec<(Pubkey, (Slot, U))>>,
    /// pubkeys with more than 1 entry
    duplicates: Mutex<StartupInfoDuplicates<T>>,
}

impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> InMemAccountsIndex<T, U> {
    pub fn new(
        storage: &Arc<BucketMapHolder<T, U>>,
        bin: usize,
        num_initial_accounts: Option<usize>,
    ) -> Self {
        let num_ages_to_distribute_flushes = Age::MAX - storage.ages_to_stay_in_cache;
        let bin_calc = PubkeyBinCalculator24::new(storage.bins);
        let lowest_pubkey = bin_calc.lowest_pubkey_from_bin(bin);
        let highest_pubkey = bin_calc.highest_pubkey_from_bin(bin);

        let map_internal = if let Some(num_initial_accounts) = num_initial_accounts {
            let capacity_per_bin = num_initial_accounts / storage.bins;
            RwLock::new(HashMap::with_capacity_and_hasher(
                capacity_per_bin,
                ahash::RandomState::default(),
            ))
        } else {
            RwLock::default()
        };

        Self {
            map_internal,
            storage: Arc::clone(storage),
            _bin: bin,
            lowest_pubkey,
            highest_pubkey,
            bucket: storage
                .disk
                .as_ref()
                .map(|disk| disk.get_bucket_from_index(bin))
                .cloned(),
            flushing_active: AtomicBool::default(),
            // initialize this to max, to make it clear we have not flushed at age 0, the starting age
            last_age_flushed: AtomicAge::new(Age::MAX),
            startup_info: StartupInfo::default(),
            // Spread out the scanning across all ages within the window.
            // This causes us to scan 1/N of the bins each 'Age'
            remaining_ages_to_skip_flushing: AtomicAge::new(
                rng().random_range(0..num_ages_to_distribute_flushes),
            ),
            num_ages_to_distribute_flushes,
            startup_stats: Arc::clone(&storage.startup_stats),
        }
    }

    /// true if this bucket needs to call flush for the current age
    /// we need to scan each bucket once per value of age
    fn get_should_age(&self, age: Age) -> bool {
        let last_age_flushed = self.last_age_flushed();
        last_age_flushed != age
    }

    /// called after flush scans this bucket at the current age
    fn set_has_aged(&self, age: Age, can_advance_age: bool) {
        self.last_age_flushed.store(age, Ordering::Release);
        self.storage.bucket_flushed_at_current_age(can_advance_age);
    }

    fn last_age_flushed(&self) -> Age {
        self.last_age_flushed.load(Ordering::Acquire)
    }

    /// return all keys in this bin
    pub fn keys(&self) -> Vec<Pubkey> {
        Self::update_stat(&self.stats().keys, 1);

        // Collect keys from the in-memory map first.
        let mut keys: HashSet<_> = self.map_internal.read().unwrap().keys().cloned().collect();

        // Next, collect keys from the disk.
        if let Some(disk) = self.bucket.as_ref() {
            let disk_keys = disk.keys();
            keys.reserve(disk_keys.len());
            for key in disk_keys {
                keys.insert(key);
            }
        }
        keys.into_iter().collect()
    }

    fn load_from_disk(&self, pubkey: &Pubkey) -> Option<(SlotList<U>, RefCount)> {
        self.bucket.as_ref().and_then(|disk| {
            let m = Measure::start("load_disk_found_count");
            let entry_disk = disk.read_value(pubkey);
            match &entry_disk {
                Some(_) => {
                    Self::update_time_stat(&self.stats().load_disk_found_us, m);
                    Self::update_stat(&self.stats().load_disk_found_count, 1);
                }
                None => {
                    Self::update_time_stat(&self.stats().load_disk_missing_us, m);
                    Self::update_stat(&self.stats().load_disk_missing_count, 1);
                }
            }
            entry_disk.map(|(slot_list, ref_count)| {
                // SAFETY: ref_count must've come from in-mem first, so converting back is safe.
                (slot_list, ref_count as RefCount)
            })
        })
    }

    /// lookup 'pubkey' in disk map.
    /// If it is found, convert it to a cache entry and return the cache entry.
    /// Cache entries from this function will always not be dirty.
    fn load_account_entry_from_disk(&self, pubkey: &Pubkey) -> Option<AccountMapEntry<T>> {
        let entry_disk = self.load_from_disk(pubkey)?; // returns None if not on disk
        let entry_cache = self.disk_to_cache_entry(entry_disk.0, entry_disk.1);
        debug_assert!(!entry_cache.dirty());
        Some(entry_cache)
    }

    /// lookup 'pubkey' by only looking in memory. Does not look on disk.
    /// callback is called whether pubkey is found or not
    pub(super) fn get_only_in_mem<RT>(
        &self,
        pubkey: &Pubkey,
        update_age: bool,
        callback: impl for<'a> FnOnce(Option<&'a AccountMapEntry<T>>) -> RT,
    ) -> RT {
        let mut found = true;
        let mut m = Measure::start("get");
        let result = {
            let map = self.map_internal.read().unwrap();
            let result = map.get(pubkey);
            m.stop();

            callback(if let Some(entry) = result {
                if update_age {
                    self.set_age_to_future(entry, false);
                }
                Some(entry)
            } else {
                drop(map);
                found = false;
                None
            })
        };

        let stats = self.stats();
        let (count, time) = if found {
            (&stats.gets_from_mem, &stats.get_mem_us)
        } else {
            (&stats.gets_missing, &stats.get_missing_us)
        };
        Self::update_stat(time, m.as_us());
        Self::update_stat(count, 1);

        result
    }

    /// set age of 'entry' to the future
    /// if 'is_cached', age will be set farther
    fn set_age_to_future(&self, entry: &AccountMapEntry<T>, is_cached: bool) {
        entry.set_age(self.storage.future_age_to_flush(is_cached));
    }

    /// lookup 'pubkey' in index (in_mem or disk).
    /// call 'callback' whether found or not
    pub(super) fn get_internal_inner<RT>(
        &self,
        pubkey: &Pubkey,
        // return true if item should be added to in_mem cache
        callback: impl for<'a> FnOnce(Option<&AccountMapEntry<T>>) -> (bool, RT),
    ) -> RT {
        // SAFETY: The entry Arc is not passed to `callback`, so
        // it cannot live beyond this function call.
        self.get_only_in_mem(pubkey, true, |entry| {
            if let Some(entry) = entry {
                callback(Some(entry)).1
            } else {
                // not in cache, look on disk
                let stats = self.stats();
                let disk_entry = self.load_account_entry_from_disk(pubkey);
                if disk_entry.is_none() {
                    return callback(None).1;
                }
                let disk_entry = disk_entry.unwrap();
                let mut map = self.map_internal.write().unwrap();
                let capacity_pre = map.capacity();
                let entry = map.entry(*pubkey);
                let retval = match entry {
                    Entry::Occupied(occupied) => callback(Some(occupied.get())).1,
                    Entry::Vacant(vacant) => {
                        debug_assert!(!disk_entry.dirty());
                        let (add_to_cache, rt) = callback(Some(&disk_entry));
                        // We are holding a write lock to the in-memory map.
                        // This pubkey is not in the in-memory map.
                        // If the entry is now dirty, then it must be put in the cache or the modifications will be lost.
                        if add_to_cache || disk_entry.dirty() {
                            stats.inc_mem_count();
                            vacant.insert(Box::new(disk_entry));
                        }
                        rt
                    }
                };
                let capacity_post = map.capacity();
                drop(map);
                stats.update_in_mem_capacity(capacity_pre, capacity_post);
                retval
            }
        })
    }

    fn remove_if_slot_list_empty_value(&self, is_empty: bool) -> bool {
        if is_empty {
            self.stats().inc_delete();
            true
        } else {
            false
        }
    }

    fn delete_disk_key(&self, pubkey: &Pubkey) {
        if let Some(disk) = self.bucket.as_ref() {
            disk.delete_key(pubkey)
        }
    }

    /// return false if the entry is in the index (disk or memory) and has a slot list len > 0
    /// return true in all other cases, including if the entry is NOT in the index at all
    fn remove_if_slot_list_empty_entry(
        &self,
        entry: Entry<Pubkey, Box<AccountMapEntry<T>>>,
    ) -> bool {
        match entry {
            Entry::Occupied(occupied) => {
                let result = self
                    .remove_if_slot_list_empty_value(occupied.get().slot_list_lock_read_len() == 0);
                if result {
                    // note there is a potential race here that has existed.
                    // if someone else holds the arc,
                    //  then they think the item is still in the index and can make modifications.
                    // We have to have a write lock to the map here, which means nobody else can get
                    //  the arc, but someone may already have retrieved a clone of it.
                    // account index in_mem flushing is one such possibility
                    self.delete_disk_key(occupied.key());
                    self.stats().dec_mem_count();
                    occupied.remove();
                }
                result
            }
            Entry::Vacant(vacant) => {
                // not in cache, look on disk
                let entry_disk = self.load_from_disk(vacant.key());
                match entry_disk {
                    Some(entry_disk) => {
                        // on disk
                        if self.remove_if_slot_list_empty_value(entry_disk.0.is_empty()) {
                            // not in cache, but on disk, so just delete from disk
                            self.delete_disk_key(vacant.key());
                            true
                        } else {
                            // could insert into cache here, but not required for correctness and value is unclear
                            false
                        }
                    }
                    None => true, // not in cache or on disk, but slot list is 'empty' and entry is not in index, so return true
                }
            }
        }
    }

    // If the slot list for pubkey exists in the index and is empty, remove the index entry for pubkey and return true.
    // Return false otherwise.
    pub fn remove_if_slot_list_empty(&self, pubkey: Pubkey) -> bool {
        let mut m = Measure::start("entry");
        let mut map = self.map_internal.write().unwrap();
        let capacity_pre = map.capacity();
        let entry = map.entry(pubkey);
        m.stop();
        let found = matches!(entry, Entry::Occupied(_));
        let result = self.remove_if_slot_list_empty_entry(entry);
        let capacity_post = map.capacity();
        drop(map);
        self.stats()
            .update_in_mem_capacity(capacity_pre, capacity_post);
        self.update_entry_stats(m, found);
        result
    }

    /// call `user_fn` with a write lock of the slot list.
    /// Note that whether `user_fn` modifies the slot list or not, the entry in the in-mem index will always
    /// be marked as dirty. So, callers to this should ideally know they will be modifying the slot list.
    pub fn slot_list_mut<RT>(
        &self,
        pubkey: &Pubkey,
        user_fn: impl FnOnce(SlotListWriteGuard<T>) -> RT,
    ) -> Option<RT> {
        self.get_internal_inner(pubkey, |entry| {
            (
                true,
                entry.map(|entry| {
                    let result = user_fn(entry.slot_list_write_lock());
                    // note that to be safe here, we ALWAYS mark the entry as dirty
                    entry.set_dirty(true);
                    result
                }),
            )
        })
    }

    /// Insert a cached entry into the accounts index
    /// If the entry is already present, just mark dirty and set the age to the future
    fn cache_entry_at_slot(current: &AccountMapEntry<T>, new_value: SlotListItem<T>) {
        let mut slot_list = current.slot_list_write_lock();
        let (slot, new_entry) = new_value;
        if !slot_list
            .iter()
            .any(|(existing_slot, _)| *existing_slot == slot)
        {
            slot_list.push((slot, new_entry));
        }
        current.set_dirty(true);
    }

    pub fn upsert(
        &self,
        pubkey: &Pubkey,
        new_value: PreAllocatedAccountMapEntry<T>,
        other_slot: Option<Slot>,
        reclaims: &mut ReclaimsSlotList<T>,
        reclaim: UpsertReclaim,
    ) {
        let (slot, account_info) = new_value.into();
        let is_cached = account_info.is_cached();

        self.get_or_create_index_entry_for_pubkey(pubkey, |entry| {
            if is_cached {
                Self::cache_entry_at_slot(entry, (slot, account_info));
                self.set_age_to_future(entry, true);
            } else {
                let slot_list_length = Self::lock_and_update_slot_list(
                    entry,
                    (slot, account_info),
                    other_slot,
                    reclaims,
                    reclaim,
                );
                self.set_age_to_future(entry, slot_list_length > 1);
            }
        });
    }

    /// Gets an entry for `pubkey` and calls `callback` with it.
    /// If the entry is not in the index, an empty entry will be created and passed to `callback`.
    /// If the entry is in the index, it will be passed to `callback` as is.
    pub fn get_or_create_index_entry_for_pubkey(
        &self,
        pubkey: &Pubkey,
        callback: impl FnOnce(&AccountMapEntry<T>),
    ) {
        let mut updated_in_mem = true;
        // try to get it just from memory first using only a read lock
        self.get_only_in_mem(pubkey, false, |entry| {
            if let Some(entry) = entry {
                callback(entry);
            } else {
                let stats = self.stats();
                let mut m = Measure::start("entry");
                let mut map = self.map_internal.write().unwrap();
                let capacity_pre = map.capacity();
                let entry = map.entry(*pubkey);
                m.stop();
                let found = matches!(entry, Entry::Occupied(_));
                match entry {
                    Entry::Occupied(mut occupied) => {
                        let current = occupied.get_mut();
                        callback(current);
                    }
                    Entry::Vacant(vacant) => {
                        // not in cache, look on disk
                        updated_in_mem = false;

                        // go to in-mem cache first
                        let disk_entry = self.load_account_entry_from_disk(vacant.key());
                        let new_value = if let Some(disk_entry) = disk_entry {
                            // on disk, so merge new_value with what was on disk
                            disk_entry
                        } else {
                            // not on disk, so insert new thing
                            self.stats().inc_insert();
                            AccountMapEntry::new(
                                SlotList::new(),
                                0,
                                AccountMapEntryMeta::new_dirty(&self.storage, true),
                            )
                        };
                        callback(&new_value);

                        // Ensure that after callback there is an item in the slot list
                        assert_ne!(
                            new_value.slot_list_lock_read_len(),
                            0,
                            "Callback must insert item into slot list"
                        );
                        assert!(new_value.dirty());
                        vacant.insert(Box::new(new_value));
                        stats.inc_mem_count();
                    }
                };
                let capacity_post = map.capacity();
                drop(map);
                stats.update_in_mem_capacity(capacity_pre, capacity_post);
                self.update_entry_stats(m, found);
            };
        });
        if updated_in_mem {
            Self::update_stat(&self.stats().updates_in_mem, 1);
        }
    }

    fn update_entry_stats(&self, stopped_measure: Measure, found: bool) {
        let stats = self.stats();
        let (count, time) = if found {
            (&stats.entries_from_mem, &stats.entry_mem_us)
        } else {
            (&stats.entries_missing, &stats.entry_missing_us)
        };
        Self::update_stat(time, stopped_measure.as_us());
        Self::update_stat(count, 1);
    }

    /// Try to update an item in the slot list the given `slot` If an item for the slot
    /// already exists in the list, remove the older item, add it to `reclaims`, and insert
    /// the new item.
    /// if 'other_slot' is some, then remove any entries in the slot list at 'other_slot' instead
    /// if UpsertReclaim is RemoveOldSlots, remove all uncached slots older than 'slot'
    /// and add them to reclaims
    /// Note:: This function only supports uncached types `T`.
    fn lock_and_update_slot_list(
        current: &AccountMapEntry<T>,
        new_value: SlotListItem<T>,
        other_slot: Option<Slot>,
        reclaims: &mut ReclaimsSlotList<T>,
        reclaim: UpsertReclaim,
    ) -> usize {
        let mut slot_list = current.slot_list_write_lock();
        let (slot, new_entry) = new_value;
        let (ref_count_change, slot_list_len) = Self::update_slot_list(
            &mut slot_list,
            slot,
            new_entry,
            other_slot,
            reclaims,
            reclaim,
        );

        match ref_count_change.cmp(&0) {
            cmp::Ordering::Equal => {
                // Do nothing
            }
            cmp::Ordering::Greater => {
                // If the ref count change is positive, it must be 1 as only one entry is being added
                assert_eq!(ref_count_change, 1);
                current.addref();
            }
            cmp::Ordering::Less => {
                current.unref_by_count(ref_count_change.unsigned_abs());
            }
        }
        current.set_dirty(true);
        slot_list_len
    }

    /// Modifies the slot_list by replacing or appending entries.
    ///
    /// - Replaces any entry at `slot` or `other_slot` with `account_info`.
    /// - Appends `account_info` to the slot list if `slot` did not exist previously.
    /// - If UpsertReclaim is ReclaimOldSlots, remove all uncached entries older than `slot`
    ///   and add them to reclaims
    ///
    /// Returns the reference count change as an `i32` and the final length of the slot list.
    /// The reference count change is the number of entries added (1) - the number of uncached
    /// entries removed or replaced
    fn update_slot_list(
        slot_list: &mut SlotListWriteGuard<T>,
        slot: Slot,
        account_info: T,
        other_slot: Option<Slot>,
        reclaims: &mut ReclaimsSlotList<T>,
        reclaim: UpsertReclaim,
    ) -> (i32, usize) {
        let mut ref_count_change = 1;

        // Cached accounts are not expected by this function, use cache_entry_at_slot instead
        assert!(!account_info.is_cached());

        let old_slot = other_slot.unwrap_or(slot);

        // If we find an existing account at old_slot, replace it rather than adding a new entry to the list
        let mut found_slot = false;
        let mut final_len = slot_list.retain_and_count(|cur_item| {
            let (cur_slot, cur_account_info) = cur_item;
            if *cur_slot == old_slot {
                // Ensure we only find one!
                assert!(!found_slot);
                let is_cur_account_cached = cur_account_info.is_cached();

                // Replace the item
                let reclaim_item = mem::replace(cur_item, (slot, account_info));
                match reclaim {
                    UpsertReclaim::ReclaimOldSlots | UpsertReclaim::PopulateReclaims => {
                        // Reclaims are used to reclaim other versions of accounts when they are
                        // rewritten elsewhere. Cached accounts are not in storage, so there is
                        // no reason to store the reclaim.
                        if !is_cur_account_cached {
                            reclaims.push(reclaim_item);
                        }
                    }
                    UpsertReclaim::PreviousSlotEntryWasCached => {
                        assert!(is_cur_account_cached);
                    }
                    UpsertReclaim::IgnoreReclaims => {
                        // do nothing. nothing to assert. nothing to return in reclaims
                    }
                }

                found_slot = true;

                if !is_cur_account_cached {
                    // current info at 'slot' is NOT cached, so we should NOT addref. This slot already has a ref count for this pubkey.
                    ref_count_change -= 1
                }
            } else if reclaim == UpsertReclaim::ReclaimOldSlots {
                let is_cur_account_cached = cur_account_info.is_cached();
                if !is_cur_account_cached && *cur_slot < slot {
                    reclaims.push(*cur_item);
                    ref_count_change -= 1;
                    return false;
                }
            } else {
                // Slot is new item that is being added to the slot list
                // If slot is already in the slot list, it must be replaced otherwise it will
                // lead to the same slot being duplicated in the list
                assert_ne!(
                    *cur_slot, slot,
                    "slot_list has slot in slot_list but is not replacing it"
                );
            }
            true
        });

        if !found_slot {
            // if we make it here, we did not find the slot in the list
            slot_list.push((slot, account_info));
            final_len += 1;
        }
        (ref_count_change, final_len)
    }

    // convert from raw data on disk to AccountMapEntry, set to age in future
    fn disk_to_cache_entry(
        &self,
        slot_list: SlotList<U>,
        ref_count: RefCount,
    ) -> AccountMapEntry<T> {
        AccountMapEntry::new(
            slot_list
                .into_iter()
                .map(|(slot, info)| (slot, info.into()))
                .collect(),
            ref_count,
            AccountMapEntryMeta::new_clean(&self.storage),
        )
    }

    /// Queue up these insertions for when the flush thread is dealing with this bin.
    /// This is very fast and requires no lookups or disk access.
    pub fn startup_insert_only(
        &self,
        slot: Slot,
        items: impl ExactSizeIterator<Item = (Pubkey, T)>,
    ) {
        assert!(self.storage.get_startup());
        assert!(self.bucket.is_some());

        let mut insert = self.startup_info.insert.lock().unwrap();
        let m = Measure::start("copy");
        insert.extend(items.map(|(k, v)| (k, (slot, v.into()))));
        self.startup_stats
            .copy_data_us
            .fetch_add(m.end_as_us(), Ordering::Relaxed);
    }

    pub fn startup_update_duplicates_from_in_memory_only(&self, items: Vec<(Slot, Pubkey)>) {
        assert!(self.storage.get_startup());
        assert!(self.bucket.is_none());

        let mut duplicates = self.startup_info.duplicates.lock().unwrap();
        duplicates.duplicates_from_in_memory_only.extend(items);
    }

    /// Upsert `new_entry` for `pubkey` into the primary index
    ///
    /// Returns info about existing entries for `pubkey`.
    ///
    /// This fn is only called at startup. The return information is used by the callers to
    /// batch-update accounts index stats.
    pub fn insert_new_entry_if_missing_with_lock(
        &self,
        pubkey: Pubkey,
        new_entry: PreAllocatedAccountMapEntry<T>,
    ) -> InsertNewEntryResults {
        let mut map = self.map_internal.write().unwrap();
        let entry = map.entry(pubkey);
        let mut other_slot = None;
        let (found_in_mem, already_existed) = match entry {
            Entry::Occupied(occupied) => {
                // in cache, so merge into cache
                let (slot, account_info) = new_entry.into();

                let slot_list = occupied.get().slot_list_read_lock();

                // If there is only one entry in the slot list, it means that
                // the previous entry inserted was a duplicate, which should be
                // added to the duplicates list too. Note that we only need to do
                // this for slot_list.len() == 1. For slot_list.len() > 1, the
                // items, previously inserted into the slot_list, have already
                // been added. We don't need to add them again.
                if slot_list.len() == 1 {
                    other_slot = Some(slot_list[0].0);
                }
                drop(slot_list);

                let updated_slot_list_len = Self::lock_and_update_slot_list(
                    occupied.get(),
                    (slot, account_info),
                    None, // should be None because we don't expect a different slot # during index generation
                    &mut ReclaimsSlotList::new(),
                    UpsertReclaim::IgnoreReclaims,
                );

                // In case of a race condition, multiple threads try to insert
                // to the same pubkey with different slots. We only need to
                // record `other_slot` once. If the slot list length after
                // update is not 2, it means that someone else has already
                // recorded `other_slot` before us. Therefore, We don't need to
                // record it again.
                if updated_slot_list_len != 2 {
                    // clear `other_slot` if we don't win the race.
                    other_slot = None;
                }

                (
                    true, /* found in mem */
                    true, /* already existed */
                )
            }
            Entry::Vacant(vacant) => {
                // not in cache, look on disk
                let disk_entry = self.load_account_entry_from_disk(vacant.key());
                if let Some(disk_entry) = disk_entry {
                    let (slot, account_info) = new_entry.into();
                    InMemAccountsIndex::<T, U>::lock_and_update_slot_list(
                        &disk_entry,
                        (slot, account_info),
                        // None because we are inserting the first element in the slot list for this pubkey.
                        // There can be no 'other' slot in the list.
                        None,
                        &mut ReclaimsSlotList::new(),
                        UpsertReclaim::IgnoreReclaims,
                    );
                    vacant.insert(Box::new(disk_entry));
                    (
                        false, /* found in mem */
                        true,  /* already existed */
                    )
                } else {
                    // not on disk, so insert new thing and we're done
                    let new_entry = new_entry.into_account_map_entry(&self.storage);
                    assert!(new_entry.dirty());
                    vacant.insert(new_entry);
                    (
                        false, /* found in mem */
                        false, /* already existed */
                    )
                }
            }
        };
        drop(map);

        if already_existed {
            let location = if found_in_mem {
                ExistedLocation::InMem
            } else {
                ExistedLocation::OnDisk
            };
            InsertNewEntryResults::Existed {
                other_slot,
                location,
            }
        } else {
            InsertNewEntryResults::DidNotExist
        }
    }

    pub fn flush(&self, can_advance_age: bool) {
        if let Some(flush_guard) = FlushGuard::lock(&self.flushing_active) {
            self.flush_internal(&flush_guard, can_advance_age)
        }
    }

    /// The footprint of a single element in the in-mem hashmap
    pub const fn size_of_uninitialized() -> usize {
        size_of::<Pubkey>() + size_of::<Box<AccountMapEntry<T>>>()
    }

    /// The size of an index value, with only a single entry in the slot list
    pub const fn size_of_single_entry() -> usize {
        size_of::<AccountMapEntry<T>>()
    }

    fn should_evict_based_on_age(
        current_age: Age,
        entry: &AccountMapEntry<T>,
        ages_flushing_now: Age,
    ) -> bool {
        current_age.wrapping_sub(entry.age()) <= ages_flushing_now
    }

    /// Returns the value to write to disk, if `entry` can be flushed.
    ///
    /// To be flushed, `entry` must be dirty and regular.
    /// ('regular' means ref count == 1 and slot list len == 1)
    ///
    /// If yes can be flushed, then `entry`'s dirty flag will be cleared.
    /// If no cannot be flushed, then `entry`'s dirty flag will remain set.
    fn try_make_entry_for_flush(
        &self,
        entry: &AccountMapEntry<T>,
        current_age: Age,
        ages_flushing_now: Age,
    ) -> ShouldFlush<SlotListItem<T>> {
        // Step 1: Perform the cheap checks on the entry
        // Step 2: Clear the dirty flag
        // Step 3: Perform all the checks on the entry.
        // - If any fail, set the dirty flag again, update stats, and return None.
        // Step 4: Extract the data to perform disk update outside the lock
        //
        // Race condition handling: If a parallel operation dirties the item again after scanning,
        // then we will set_dirty(true) and skip the disk update. The dirty flag will ensure the
        // next flush picks up the item again. If the item becomes dirty during our disk write,
        // that's ok - the dirty flag will be picked up on the next flush and prevent us from
        // evicting the item from the cache.

        if !Self::should_evict_based_on_age(current_age, entry, ages_flushing_now) {
            // entry was bumped in age after initial scan for candidates
            // do not flush now; will be handled in later passes (at later ages)
            return ShouldFlush::No(ReasonToNotFlush::Age);
        }

        if entry.ref_count() != 1 {
            // we only flush regular entries, i.e. ref count == 1
            return ShouldFlush::No(ReasonToNotFlush::RefCount);
        }

        // assume we're going to flush this entry, so clear its dirty flag
        let was_dirty = entry.clear_dirty();
        if !was_dirty {
            // entry is not dirty anymore, skip disk write
            return ShouldFlush::No(ReasonToNotFlush::Clean);
        }

        // lock the slot list and then check *everything*
        // if a check fails, do not flush, and set dirty flag again
        let slot_list = entry.slot_list_read_lock();

        // re-check the ref count after locking the slot list
        if entry.ref_count() != 1 {
            entry.set_dirty(true);
            return ShouldFlush::No(ReasonToNotFlush::RefCount);
        }

        if slot_list.len() != 1 {
            // we only flush regular entries, i.e. slot list len == 1
            entry.set_dirty(true);
            return ShouldFlush::No(ReasonToNotFlush::SlotListLen);
        }

        // SAFETY: We just checked that the slot list len is 1
        let slot_list_elem = slot_list[0];

        if slot_list_elem.1.is_cached() {
            // we only flush regular entries, i.e. slot list does not contain any cached entries
            entry.set_dirty(true);
            return ShouldFlush::No(ReasonToNotFlush::SlotListCached);
        }

        // entry is ready to be flushed
        ShouldFlush::Yes(slot_list_elem)
    }

    /// Collect candidates to flush/evict from `iter` by checking age
    /// Skip entries with ref_count != 1 since they will be rejected later anyway
    fn gather_possible_evictions<'a>(
        iter: impl Iterator<Item = (&'a Pubkey, &'a Box<AccountMapEntry<T>>)>,
        current_age: Age,
        ages_flushing_now: Age,
        max_evictions: NonZeroUsize,
    ) -> (CandidatesToFlush, CandidatesToEvict) {
        let mut candidates_to_flush = Vec::new();
        let mut candidates_to_evict = Vec::new();
        let mut rng = rng();
        // use reservoir sampling to select a bounded, roughly uniform subset
        let mut sampling_state = ReservoirState {
            samples: Vec::with_capacity(max_evictions.get()),
            seen: 0,
            max_samples: max_evictions,
        };
        for (k, v) in iter {
            if !Self::should_evict_based_on_age(current_age, v, ages_flushing_now) {
                // not planning to evict this item from memory within 'ages_flushing_now' ages
                continue;
            }

            // Skip entries with ref_count != 1 early
            // In 99% of cases, these will be rejected by try_make_entry_for_flush or evict_from_cache anyway
            // Filtering here avoids unnecessary work and reduces write lock contention in evict_from_cache
            if v.ref_count() != 1 {
                continue;
            }
            sampling_state.select(
                CandidateSelection {
                    key: *k,
                    dirty: v.dirty(),
                },
                &mut rng,
            );
        }

        for candidate in sampling_state.samples {
            if candidate.dirty {
                candidates_to_flush.push(candidate.key);
            } else {
                candidates_to_evict.push(candidate.key);
            }
        }
        (
            CandidatesToFlush(candidates_to_flush),
            CandidatesToEvict(candidates_to_evict),
        )
    }

    /// scan loop
    /// holds read lock
    /// Returns candidates to flush/evict now, pending further checks.
    /// Entries with ref_count != 1 are filtered out during scan
    fn flush_scan(
        &self,
        current_age: Age,
        _flush_guard: &FlushGuard,
        ages_flushing_now: Age,
    ) -> (CandidatesToFlush, CandidatesToEvict) {
        let (possible_evictions, m) = {
            let map = self.map_internal.read().unwrap();
            let m = Measure::start("flush_scan"); // we don't care about lock time in this metric - bg threads can wait
            let max_evictions = NonZeroUsize::new(map.len().max(1)).unwrap();
            let possible_evictions = Self::gather_possible_evictions(
                map.iter(),
                current_age,
                ages_flushing_now,
                max_evictions,
            );
            (possible_evictions, m)
        };
        Self::update_time_stat(&self.stats().flush_scan_us, m);

        possible_evictions
    }

    fn write_startup_info_to_disk(&self) {
        let insert = std::mem::take(&mut *self.startup_info.insert.lock().unwrap());
        if insert.is_empty() {
            // nothing to insert for this bin
            return;
        }

        // during startup, nothing should be in the in-mem map
        let map_internal = self.map_internal.read().unwrap();
        assert!(
            map_internal.is_empty(),
            "len: {}, first: {:?}",
            map_internal.len(),
            map_internal.iter().take(1).collect::<Vec<_>>()
        );
        drop(map_internal);

        // this fn should only be called from a single thread, so holding the lock is fine
        let mut duplicates = self.startup_info.duplicates.lock().unwrap();

        // merge all items into the disk index now
        let disk = self.bucket.as_ref().unwrap();
        let mut count = insert.len() as u64;
        for (i, duplicate_entry) in disk.batch_insert_non_duplicates(&insert) {
            let (k, entry) = &insert[i];
            duplicates.duplicates.push((entry.0, *k, entry.1.into()));
            // accurately account for there being a duplicate for the first entry that was previously added to the disk index.
            // That entry could not have known yet that it was a duplicate.
            // It is important to capture each slot with a duplicate because of slot limits applied to clean.
            duplicates
                .duplicates_put_on_disk
                .insert((duplicate_entry.0, *k));
            count -= 1;
        }

        self.stats().inc_insert_count(count);
    }

    /// pull out all duplicate pubkeys from 'startup_info'
    /// duplicate pubkeys have a slot list with len > 1
    /// These were collected for this bin when we did batch inserts in the bg flush threads.
    /// Insert these into the in-mem index, then return the duplicate (Slot, Pubkey)
    pub fn populate_and_retrieve_duplicate_keys_from_startup(&self) -> Vec<(Slot, Pubkey)> {
        // in order to return accurate and complete duplicates, we must have nothing left remaining to insert
        assert!(self.startup_info.insert.lock().unwrap().is_empty());

        let mut duplicate_items = self.startup_info.duplicates.lock().unwrap();
        let duplicates = std::mem::take(&mut duplicate_items.duplicates);
        let duplicates_put_on_disk = std::mem::take(&mut duplicate_items.duplicates_put_on_disk);
        drop(duplicate_items);

        // accumulated stats after inserting pubkeys into the index
        let mut num_did_not_exist = 0;
        let mut num_existed_in_mem = 0;
        let mut num_existed_on_disk = 0;

        let storage = self.storage.as_ref();
        let duplicates = duplicates_put_on_disk
            .into_iter()
            .chain(duplicates.into_iter().map(|(slot, key, info)| {
                let entry = PreAllocatedAccountMapEntry::new(slot, info, storage, true);
                match self.insert_new_entry_if_missing_with_lock(key, entry) {
                    InsertNewEntryResults::DidNotExist => {
                        num_did_not_exist += 1;
                    }
                    InsertNewEntryResults::Existed {
                        other_slot: _,
                        location,
                    } => match location {
                        ExistedLocation::InMem => {
                            num_existed_in_mem += 1;
                        }
                        ExistedLocation::OnDisk => {
                            num_existed_on_disk += 1;
                        }
                    },
                };
                (slot, key)
            }))
            .collect();

        let stats = self.stats();

        // stats for inserted entries that previously did *not* exist
        stats.inc_insert_count(num_did_not_exist);
        stats.add_mem_count(num_did_not_exist as usize);

        // stats for inserted entries that previous did exist *in-mem*
        stats
            .entries_from_mem
            .fetch_add(num_existed_in_mem, Ordering::Relaxed);
        stats
            .updates_in_mem
            .fetch_add(num_existed_in_mem, Ordering::Relaxed);

        // stats for inserted entries that previously did exist *on-disk*
        stats.add_mem_count(num_existed_on_disk as usize);
        stats
            .entries_missing
            .fetch_add(num_existed_on_disk, Ordering::Relaxed);
        stats
            .updates_in_mem
            .fetch_add(num_existed_on_disk, Ordering::Relaxed);

        duplicates
    }

    pub fn startup_take_duplicates_from_in_memory_only(&self) -> Vec<(Slot, Pubkey)> {
        let mut duplicates = self.startup_info.duplicates.lock().unwrap();
        std::mem::take(&mut duplicates.duplicates_from_in_memory_only)
    }

    /// synchronize the in-mem index with the disk index
    fn flush_internal(&self, flush_guard: &FlushGuard, can_advance_age: bool) {
        let current_age = self.storage.current_age();
        let iterate_for_age = self.get_should_age(current_age);
        let startup = self.storage.get_startup();

        if startup {
            // At startup we do not insert index entries into the normal in-mem index.
            // Instead, they are written to a startup-only struct.  Thus, at startup
            // we only need to flush that startup struct and then can return early.
            self.write_startup_info_to_disk();
            if iterate_for_age {
                // Note we still have to iterate ages too, since it is checked when
                // transitioning from startup back to normal/steady state.
                assert_eq!(current_age, self.storage.current_age());
                self.set_has_aged(current_age, can_advance_age);
            }
            return;
        }

        // from this point forward, we know startup == false
        debug_assert!(!startup);

        if !iterate_for_age {
            // no need to age, so no need to flush this bucket
            return;
        }

        // from this point forward, we know iterate_for_age == true
        debug_assert!(iterate_for_age);

        let ages_flushing_now = {
            let old_value = self
                .remaining_ages_to_skip_flushing
                .fetch_sub(1, Ordering::AcqRel);
            if old_value == 0 {
                self.remaining_ages_to_skip_flushing
                    .store(self.num_ages_to_distribute_flushes, Ordering::Release);
            } else {
                // skipping iteration of the buckets at the current age, but mark the bucket as having aged
                assert_eq!(current_age, self.storage.current_age());
                self.set_has_aged(current_age, can_advance_age);
                return;
            }
            self.num_ages_to_distribute_flushes
        };

        Self::update_stat(&self.stats().buckets_scanned, 1);

        // scan in-mem map for candidates to flush/evict
        let (candidates_to_flush, candidates_to_evict) =
            self.flush_scan(current_age, flush_guard, ages_flushing_now);

        // write to disk outside in-mem map read lock
        let disk = self.bucket.as_ref().unwrap();
        let mut flush_stats = DiskFlushStats::new();

        // Process each candidate to flush
        // For each entry: lock map briefly, get entry, calculate disk value, release lock, then write to disk
        let flush_update_measure = Measure::start("flush_update");
        let flushed_keys_to_evict: Vec<_> = candidates_to_flush
            .0
            .into_iter()
            .filter_map(|key| {
                // Entry was dirty at scan time, need to write to disk
                let lock_measure = Measure::start("flush_read_lock");
                let map_read_guard = self.map_internal.read().unwrap();
                let entry = map_read_guard.get(&key)?;

                let mse = Measure::start("flush_should_evict");
                let maybe_entry_for_flush =
                    self.try_make_entry_for_flush(entry, current_age, ages_flushing_now);
                flush_stats.flush_should_evict_us += mse.end_as_us();

                drop(map_read_guard);
                flush_stats.flush_read_lock_us += lock_measure.end_as_us();

                let (slot, account_info) = match maybe_entry_for_flush {
                    ShouldFlush::Yes(entry_for_flush) => entry_for_flush,
                    ShouldFlush::No(reason) => {
                        match reason {
                            ReasonToNotFlush::Clean => flush_stats.num_not_flushed_clean += 1,
                            ReasonToNotFlush::Age => flush_stats.num_not_flushed_age += 1,
                            ReasonToNotFlush::RefCount => {
                                flush_stats.num_not_flushed_ref_count += 1
                            }
                            ReasonToNotFlush::SlotListLen => {
                                flush_stats.num_not_flushed_slot_list_len += 1
                            }
                            ReasonToNotFlush::SlotListCached => {
                                flush_stats.num_not_flushed_slot_list_cached += 1
                            }
                        }
                        return None;
                    }
                };
                let disk_entry = [(slot, account_info.into())];

                // Now write to disk WITHOUT holding any locks
                // may have to loop if disk has to grow and we have to retry the write
                loop {
                    let disk_resize = disk.try_write(&key, (&disk_entry, /*ref count*/ 1));
                    match disk_resize {
                        Ok(_) => {
                            // successfully written to disk
                            flush_stats.flush_entries_updated_on_disk += 1;
                            break;
                        }
                        Err(err) => {
                            // disk needs to resize. This item did not get written. Resize and try again.
                            let m = Measure::start("flush_grow");
                            disk.grow(err);
                            flush_stats.flush_grow_us += m.end_as_us();
                        }
                    }
                }

                Some(key)
            })
            .collect();
        flush_stats.flush_update_us = flush_update_measure.end_as_us();
        flush_stats.update_to_stats(self.stats());

        let m = Measure::start("flush_evict");
        self.evict_from_cache(&flushed_keys_to_evict, current_age, ages_flushing_now);
        self.evict_from_cache(&candidates_to_evict.0, current_age, ages_flushing_now);
        Self::update_time_stat(&self.stats().flush_evict_us, m);

        if iterate_for_age {
            // completed iteration of the buckets at the current age
            assert_eq!(current_age, self.storage.current_age());
            self.set_has_aged(current_age, can_advance_age);
        }
    }

    // evict keys in 'evictions' from in-mem cache, likely due to age
    fn evict_from_cache(&self, evictions: &[Pubkey], current_age: Age, ages_flushing_now: Age) {
        if evictions.is_empty() {
            return;
        }

        let stats = self.stats();
        let mut failed = 0;
        let mut evicted = 0;
        // chunk these so we don't hold the write lock too long
        for evictions in evictions.chunks(50) {
            let mut map = self.map_internal.write().unwrap();
            let capacity_pre = map.capacity();
            for k in evictions {
                if let Entry::Occupied(occupied) = map.entry(*k) {
                    let v = occupied.get();

                    if v.dirty()
                        || !Self::should_evict_based_on_age(current_age, v, ages_flushing_now)
                    {
                        // marked dirty or bumped in age after we looked above
                        // these evictions will be handled in later passes (at later ages)
                        failed += 1;
                        continue;
                    }

                    // all conditions for eviction succeeded, so really evict item from in-mem cache
                    evicted += 1;
                    occupied.remove();
                }
            }
            let capacity_post = map.capacity();
            drop(map);
            stats.update_in_mem_capacity(capacity_pre, capacity_post);
        }
        stats.sub_mem_count(evicted);
        Self::update_stat(&stats.flush_entries_evicted_from_mem, evicted as u64);
        Self::update_stat(&stats.failed_to_evict, failed as u64);
    }

    pub fn stats(&self) -> &Stats {
        &self.storage.stats
    }

    fn update_stat(stat: &AtomicU64, value: u64) {
        if value != 0 {
            stat.fetch_add(value, Ordering::Relaxed);
        }
    }

    pub fn update_time_stat(stat: &AtomicU64, mut m: Measure) {
        m.stop();
        let value = m.as_us();
        Self::update_stat(stat, value);
    }

    /// Returns the capacity for this bin's map
    ///
    /// Only intended to be called at startup, since it grabs the map's read lock.
    pub(crate) fn capacity_for_startup(&self) -> usize {
        self.map_internal.read().unwrap().capacity()
    }
}

/// Candidate tracked during reservoir sampling to flush or evict.
#[derive(Debug, Clone)]
struct CandidateSelection {
    key: Pubkey,
    dirty: bool,
}

/// State of reservoir sampling algorithm for flush/eviction candidates.
#[derive(Debug)]
struct ReservoirState {
    samples: Vec<CandidateSelection>,
    seen: usize,
    max_samples: NonZeroUsize,
}

impl ReservoirState {
    /// Select a candidate, keeping a bounded roughly uniform sample set.
    fn select(&mut self, candidate: CandidateSelection, rng: &mut impl Rng) {
        self.seen += 1;
        if self.samples.len() < self.max_samples.get() {
            self.samples.push(candidate);
            return;
        }

        let idx = rng.random_range(0..self.seen);
        if idx < self.max_samples.get() {
            self.samples[idx] = candidate;
        }
    }
}

/// Statistics for disk flush operations
#[derive(Debug, Default)]
struct DiskFlushStats {
    /// Time spent in flush update operation
    flush_update_us: u64,
    /// Time spent checking if entries should be evicted
    flush_should_evict_us: u64,
    /// Number of entries successfully written to disk
    flush_entries_updated_on_disk: u64,
    /// Time spent growing disk storage
    flush_grow_us: u64,
    /// Time spent holding map_internal read lock
    flush_read_lock_us: u64,
    /// Number of entries not flushed because they were clean
    num_not_flushed_clean: u64,
    /// Number of entries not flushed because they weren't old enough
    num_not_flushed_age: u64,
    /// Number of entries not flushed because ref count != 1
    num_not_flushed_ref_count: u64,
    /// Number of entries not flushed because slot list len != 1
    num_not_flushed_slot_list_len: u64,
    /// Number of entries not flushed because slot list contained a cached entry
    num_not_flushed_slot_list_cached: u64,
}

impl DiskFlushStats {
    fn new() -> Self {
        Self::default()
    }

    fn update_to_stats(&self, stats: &Stats) {
        Self::update_stat(&stats.flush_update_us, self.flush_update_us);
        Self::update_stat(&stats.flush_should_evict_us, self.flush_should_evict_us);
        Self::update_stat(
            &stats.flush_entries_updated_on_disk,
            self.flush_entries_updated_on_disk,
        );
        Self::update_stat(&stats.flush_grow_us, self.flush_grow_us);
        Self::update_stat(&stats.flush_read_lock_us, self.flush_read_lock_us);
        Self::update_stat(&stats.held_in_mem.clean, self.num_not_flushed_clean);
        Self::update_stat(&stats.held_in_mem.age, self.num_not_flushed_age);
        Self::update_stat(&stats.held_in_mem.ref_count, self.num_not_flushed_ref_count);
        Self::update_stat(
            &stats.held_in_mem.slot_list_len,
            self.num_not_flushed_slot_list_len,
        );
        Self::update_stat(
            &stats.held_in_mem.slot_list_cached,
            self.num_not_flushed_slot_list_cached,
        );
    }

    fn update_stat(stat: &AtomicU64, value: u64) {
        if value != 0 {
            stat.fetch_add(value, Ordering::Relaxed);
        }
    }
}

/// An RAII implementation of a scoped lock for the `flushing_active` atomic flag in
/// `InMemAccountsIndex`.  When this structure is dropped (falls out of scope), the flag will be
/// cleared (set to false).
///
/// After successfully locking (calling `FlushGuard::lock()`), pass a reference to the `FlashGuard`
/// instance to any function/code that requires the `flushing_active` flag has been set (to true).
#[derive(Debug)]
struct FlushGuard<'a> {
    flushing: &'a AtomicBool,
}

impl<'a> FlushGuard<'a> {
    /// Set the `flushing` atomic flag to true.  If the flag was already true, then return `None`
    /// (so as to not clear the flag erroneously).  Otherwise return `Some(FlushGuard)`.
    #[must_use = "if unused, the `flushing` flag will immediately clear"]
    fn lock(flushing: &'a AtomicBool) -> Option<Self> {
        let already_flushing = flushing.swap(true, Ordering::AcqRel);
        // Eager evaluation here would result in dropping Self and clearing flushing flag
        #[allow(clippy::unnecessary_lazy_evaluations)]
        (!already_flushing).then(|| Self { flushing })
    }
}

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        self.flushing.store(false, Ordering::Release);
    }
}

/// Candidates in the in-mem index that may be flushed to disk, pending further checks.
///
/// Note, entries must be 'dirty' to be a candidate for flush.
#[derive(Debug)]
struct CandidatesToFlush(Vec<Pubkey>);

/// Candidates in the in-mem index that may be evicted, pending further checks.
///
/// Note, entries must be 'clean' to be a candidate for eviction.
#[derive(Debug)]
struct CandidatesToEvict(Vec<Pubkey>);

/// Should an entry be flushed to disk?
#[derive(Debug, Eq, PartialEq)]
enum ShouldFlush<T> {
    /// No, do not flush this entry to disk.
    /// See inner `ReasonToNotFlush` for why.
    No(ReasonToNotFlush),
    /// Yes, flush this entry to disk.
    Yes(T),
}

/// Why was an entry *not* flushed to disk?
#[derive(Debug, Eq, PartialEq)]
enum ReasonToNotFlush {
    /// Only dirty entries are flushed to disk.
    Clean,
    /// This entry isn't old enough to flush yet.
    Age,
    /// return count was != 1
    /// This account has versions in multiple storages, and will be cleaned/shrunk soon.
    RefCount,
    /// slot list len was != 1
    /// This account has versions in multiple locations, and will be cleaned/shrunk soon.
    SlotListLen,
    /// slot list contained an item pointing to a cached account
    /// An account in the write cache will be flushed soon, so do not flush this index entry yet,
    /// as it will be modified soon.
    SlotListCached,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::accounts_index::{AccountsIndexConfig, IndexLimit, BINS_FOR_TESTING},
        assert_matches::assert_matches,
        itertools::Itertools,
        test_case::test_case,
    };

    fn new_for_test<T: IndexValue>() -> InMemAccountsIndex<T, T> {
        let holder = Arc::new(BucketMapHolder::new(
            BINS_FOR_TESTING,
            &AccountsIndexConfig::default(),
            1,
        ));
        let bin = 0;
        InMemAccountsIndex::new(&holder, bin, None)
    }

    fn new_disk_buckets_for_test<T: IndexValue>() -> InMemAccountsIndex<T, T> {
        let config = AccountsIndexConfig {
            index_limit: IndexLimit::Minimal,
            ..Default::default()
        };
        let holder = Arc::new(BucketMapHolder::new(BINS_FOR_TESTING, &config, 1));
        let bin = 0;
        let bucket = InMemAccountsIndex::new(&holder, bin, None);
        assert!(bucket.storage.is_disk_index_enabled());
        bucket
    }

    #[test]
    fn test_get_or_create_index_entry_for_pubkey_insert_new() {
        let accounts_index = new_for_test::<u64>();
        let pubkey = solana_pubkey::new_rand();
        let slot = 0;

        let mut callback_called = false;
        accounts_index.get_or_create_index_entry_for_pubkey(&pubkey, |entry| {
            assert_eq!(entry.slot_list_lock_read_len(), 0);
            assert_eq!(entry.ref_count(), 0);
            assert!(entry.dirty());
            InMemAccountsIndex::<u64, u64>::cache_entry_at_slot(entry, (slot, 0));
            callback_called = true;
        });

        assert!(callback_called);

        // Ensure the entry is now in memory
        let mut found = false;
        accounts_index.get_only_in_mem(&pubkey, false, |entry| {
            found = entry.is_some();
        });
        assert!(found);
    }

    #[test]
    fn test_get_or_create_index_entry_for_pubkey_existing_in_mem() {
        let accounts_index = new_for_test::<u64>();
        let pubkey = solana_pubkey::new_rand();

        // Insert an entry manually
        let entry = Box::new(AccountMapEntry::new(
            SlotList::from([(0, 42)]),
            1,
            AccountMapEntryMeta::new_dirty(&accounts_index.storage, true),
        ));
        accounts_index
            .map_internal
            .write()
            .unwrap()
            .insert(pubkey, entry);

        let mut callback_called = false;
        accounts_index.get_or_create_index_entry_for_pubkey(&pubkey, |entry| {
            assert_eq!(entry.slot_list_lock_read_len(), 1);
            assert_eq!(entry.ref_count(), 1);
            assert!(entry.dirty());
            callback_called = true;
        });

        assert!(callback_called);
    }

    #[test]
    fn test_get_or_create_index_entry_for_pubkey_existing_on_disk() {
        let accounts_index = new_disk_buckets_for_test::<u64>();
        let pubkey = solana_pubkey::new_rand();
        let slot = 0;

        // Simulate an entry on disk
        let disk_entry: (&[(u64, u64)], u64) = (&[(0u64, 42u64)], 1u64);
        accounts_index
            .bucket
            .as_ref()
            .unwrap()
            .try_write(&pubkey, disk_entry)
            .unwrap();

        // Ensure the entry is not found in memory
        let mut found = false;
        accounts_index.get_only_in_mem(&pubkey, false, |entry| {
            found = entry.is_some();
        });
        assert!(!found);

        let mut callback_called = false;
        accounts_index.get_or_create_index_entry_for_pubkey(&pubkey, |entry| {
            assert_eq!(entry.slot_list_lock_read_len(), 1);
            assert_eq!(entry.ref_count(), 1);
            assert!(!entry.dirty()); // Entry loaded from disk should not be dirty
            InMemAccountsIndex::<u64, u64>::cache_entry_at_slot(entry, (slot, 0));
            callback_called = true;
        });

        assert!(callback_called);

        // Ensure the entry is now in memory
        let mut found = false;
        accounts_index.get_only_in_mem(&pubkey, false, |entry| {
            found = entry.is_some();
        });
        assert!(found);
    }

    #[test]
    #[should_panic(
        expected = "assertion `left != right` failed: Callback must insert item into slot list"
    )]
    fn test_get_or_create_index_entry_for_pubkey_empty_slot_list_assertion() {
        let accounts_index = new_for_test::<u64>();
        let pubkey = solana_pubkey::new_rand();

        accounts_index.get_or_create_index_entry_for_pubkey(&pubkey, |_entry| {
            // Do not modify the slot list, which should trigger the assertion
        });
    }

    #[test]
    fn test_update_slot_list_other_populate_reclaims() {
        agave_logger::setup();
        let reclaim = UpsertReclaim::PopulateReclaims;
        let new_slot = 5;
        let info = 1;
        let other_value = info + 1;
        let at_new_slot = (new_slot, info);
        let unique_other_slot = new_slot + 1;
        for other_slot in [Some(new_slot), Some(unique_other_slot), None] {
            let mut reclaims = ReclaimsSlotList::new();
            let entry = AccountMapEntry::empty_for_tests();
            let mut slot_list = entry.slot_list_write_lock();
            // upserting into empty slot_list, so always addref
            assert_eq!(
                InMemAccountsIndex::<u64, u64>::update_slot_list(
                    &mut slot_list,
                    new_slot,
                    info,
                    other_slot,
                    &mut reclaims,
                    reclaim
                ),
                (1, 1),
                "other_slot: {other_slot:?}"
            );
            assert_eq!(slot_list.clone_list(), SlotList::from([at_new_slot]));
            assert!(reclaims.is_empty());
        }

        // replace other
        let entry = AccountMapEntry::new(
            SlotList::from([(unique_other_slot, other_value)]),
            1,
            AccountMapEntryMeta::default(),
        );
        let mut slot_list = entry.slot_list_write_lock();
        let expected_reclaims = ReclaimsSlotList::from(slot_list.as_ref());
        let other_slot = Some(unique_other_slot);
        let mut reclaims = ReclaimsSlotList::new();
        assert_eq!(
            // upserting into slot_list that does NOT contain an entry at 'new_slot'
            // but, it DOES contain an entry at other_slot, so we do NOT add-ref. The assumption is that 'other_slot' is going away
            // and that the previously held add-ref is now used by 'new_slot'
            InMemAccountsIndex::<u64, u64>::update_slot_list(
                &mut slot_list,
                new_slot,
                info,
                other_slot,
                &mut reclaims,
                reclaim
            ),
            (0, 1),
            "other_slot: {other_slot:?}"
        );
        assert_eq!(slot_list.clone_list(), SlotList::from([at_new_slot]));
        assert_eq!(reclaims, expected_reclaims);

        // nothing will exist at this slot
        let missing_other_slot = unique_other_slot + 1;
        let ignored_slot = 10; // bigger than is used elsewhere in the test
        let ignored_value = info + 10;

        // build a list of possible contents in the slot_list prior to calling 'update_slot_list'
        let possible_initial_slot_list_contents = {
            let mut possible_initial_slot_list_contents = Vec::new();

            // Add ignored slot account_info entries (slots with larger slot #s than 'new_slot' or 'other_slot')
            possible_initial_slot_list_contents
                .extend((0..3).map(|i| (ignored_slot + i, ignored_value + i)));

            // Add account_info for 'new_slot'
            possible_initial_slot_list_contents.push(at_new_slot);
            // Add account_info for 'other_slot'
            possible_initial_slot_list_contents.push((unique_other_slot, other_value));
            possible_initial_slot_list_contents
        };

        /*
         * loop over all possible permutations of 'possible_initial_slot_list_contents'
         * some examples:
         * []
         * [other]
         * [other, new_slot]
         * [new_slot, other]
         * [dummy0, new_slot, dummy1, other] (and all permutation of this order)
         * [other, dummy1, new_slot] (and all permutation of this order)
         * ...
         * [dummy0, new_slot, dummy1, other_slot, dummy2] (and all permutation of this order)
         */
        let mut attempts = 0;
        // loop over each initial size of 'slot_list'
        for initial_slot_list_len in 0..=possible_initial_slot_list_contents.len() {
            // loop over every permutation of possible_initial_slot_list_contents within a list of len 'initial_slot_list_len'
            for content_source_indexes in
                (0..possible_initial_slot_list_contents.len()).permutations(initial_slot_list_len)
            {
                // loop over each possible parameter for 'other_slot'
                for other_slot in [
                    Some(new_slot),
                    Some(unique_other_slot),
                    Some(missing_other_slot),
                    None,
                ] {
                    if other_slot.is_some()
                        && new_slot != other_slot.unwrap()
                        && slot_list.contains(&(new_slot, info))
                    {
                        // skip this permutation if 'new_slot' is already in the slot_list, but we are trying to reclaim other slot
                        // This is an assert case as only one of new_slot and other_slot should be in the slot list
                        continue;
                    }

                    attempts += 1;
                    // initialize slot_list prior to call to 'InMemAccountsIndex::update_slot_list'
                    // by inserting each possible entry at each possible position
                    let entry = AccountMapEntry::new(
                        content_source_indexes
                            .iter()
                            .map(|i| possible_initial_slot_list_contents[*i])
                            .collect(),
                        1,
                        AccountMapEntryMeta::default(),
                    );
                    let mut slot_list = entry.slot_list_write_lock();
                    let mut expected = slot_list.clone_list();
                    let original = slot_list.clone_list();
                    let mut reclaims = ReclaimsSlotList::new();

                    let (result, _len) = InMemAccountsIndex::<u64, u64>::update_slot_list(
                        &mut slot_list,
                        new_slot,
                        info,
                        other_slot,
                        &mut reclaims,
                        reclaim,
                    );
                    let mut slot_list = slot_list.clone_list();

                    // calculate expected reclaims
                    let mut expected_reclaims = ReclaimsSlotList::new();
                    expected.retain(|(slot, info)| {
                        let retain = slot != &new_slot && Some(*slot) != other_slot;
                        if !retain {
                            expected_reclaims.push((*slot, *info));
                        }
                        retain
                    });
                    expected.push((new_slot, info));

                    // Calculate the expected ref count change. It is expected to be 1 - the number of reclaims
                    let expected_result = 1 - expected_reclaims.len() as i32;
                    assert_eq!(
                        expected_result, result,
                        "return value different. other: {other_slot:?}, {expected:?}, \
                         {slot_list:?}, original: {original:?}"
                    );
                    // sort for easy comparison
                    expected_reclaims.sort_unstable();
                    reclaims.sort_unstable();
                    assert_eq!(
                        expected_reclaims, reclaims,
                        "reclaims different. other: {other_slot:?}, {expected:?}, {slot_list:?}, \
                         original: {original:?}"
                    );
                    // sort for easy comparison
                    slot_list.sort_unstable();
                    expected.sort_unstable();
                    assert_eq!(
                        slot_list, expected,
                        "slot_list different. other: {other_slot:?}, {expected:?}, {slot_list:?}, \
                         original: {original:?}"
                    );
                }
            }
        }
        assert_eq!(attempts, 652); // complicated permutations, so make sure we ran the right #
    }

    #[test]
    fn test_gather_possible_evictions() {
        const AGE_MAX: Age = 255;
        let ref_count = 1;
        // The values in the slot list elements do not matter.
        // They are different so we can distinguish between 'dirty' and 'clean' for the test.
        let slot_list_dirty = [(0xD1, 0xD2)];
        let slot_list_clean = [(0xC3, 0xC4)];
        let map_dirty: HashMap<_, _> = (0..=AGE_MAX)
            .map(|age| {
                let entry = Box::new(AccountMapEntry::new(
                    SlotList::from(slot_list_dirty),
                    ref_count,
                    AccountMapEntryMeta::default(),
                ));
                entry.set_dirty(true);
                entry.set_age(age);
                (Pubkey::new_unique(), entry)
            })
            .collect();
        let map_clean: HashMap<_, _> = (0..=AGE_MAX)
            .map(|age| {
                let entry = Box::new(AccountMapEntry::new(
                    SlotList::from(slot_list_clean),
                    ref_count,
                    AccountMapEntryMeta::default(),
                ));
                entry.set_dirty(false);
                entry.set_age(age);
                (Pubkey::new_unique(), entry)
            })
            .collect();

        for current_age in 0..=AGE_MAX {
            for ages_flushing_now in 0..=AGE_MAX {
                let (candidates_to_flush, candidates_to_evict) =
                    InMemAccountsIndex::<u64, u64>::gather_possible_evictions(
                        map_dirty.iter().chain(&map_clean),
                        current_age,
                        ages_flushing_now,
                        NonZeroUsize::new(map_dirty.len() + map_clean.len()).unwrap(),
                    );
                // Verify that the number of entries selected for eviction matches the expected count.
                // Test setup: map contains 256 dirty entries and 256 clean entries.
                // Each with ages 0-255 (one entry per age value).
                //
                // gather_possible_evictions includes entries where:
                //   current_age.wrapping_sub(entry.age) <= ages_flushing_now
                // which is equivalent to:
                //   entry.age >= current_age - ages_flushing_now (with wrapping)
                //
                // This selects entries in the age window [current_age - ages_flushing_now, current_age].
                // The window size is (ages_flushing_now + 1) because both endpoints are inclusive.
                //
                // Example: If current_age=10 and ages_flushing_now=3, we select ages 7,8,9,10 = 4 entries.
                assert_eq!(candidates_to_flush.0.len(), 1 + ages_flushing_now as usize);
                assert_eq!(candidates_to_evict.0.len(), 1 + ages_flushing_now as usize);
                candidates_to_flush.0.iter().for_each(|key| {
                    let entry = map_dirty.get(key).unwrap();
                    assert!(entry.dirty());
                    assert_eq!(*entry.slot_list_read_lock(), slot_list_dirty);
                    assert!(
                        InMemAccountsIndex::<u64, u64>::should_evict_based_on_age(
                            current_age,
                            entry,
                            ages_flushing_now,
                        ),
                        "current_age: {}, age: {}, ages_flushing_now: {}",
                        current_age,
                        entry.age(),
                        ages_flushing_now
                    );
                });
                candidates_to_evict.0.iter().for_each(|key| {
                    let entry = map_clean.get(key).unwrap();
                    assert!(!entry.dirty());
                    assert_eq!(*entry.slot_list_read_lock(), slot_list_clean);
                    assert!(
                        InMemAccountsIndex::<u64, u64>::should_evict_based_on_age(
                            current_age,
                            entry,
                            ages_flushing_now,
                        ),
                        "current_age: {}, age: {}, ages_flushing_now: {}",
                        current_age,
                        entry.age(),
                        ages_flushing_now
                    );
                });
            }
        }
    }

    #[test]
    fn test_gather_possible_evictions_with_max_evictions() {
        let ref_count = 1;
        let current_age = 100;
        let ages_flushing_now = 0;
        let total_entries = 256;
        let max_evictions = NonZeroUsize::new(5).unwrap();

        // Create a map with 256 entries
        let map: HashMap<_, _> = (0..total_entries)
            .map(|i| {
                let pk = Pubkey::from([i as u8; 32]);
                let one_element_slot_list = SlotList::from([(0, 0)]);
                let one_element_slot_list_entry = Box::new(AccountMapEntry::new(
                    one_element_slot_list,
                    ref_count,
                    AccountMapEntryMeta::default(),
                ));
                if i % 2 == 0 {
                    one_element_slot_list_entry.set_dirty(true);
                }
                one_element_slot_list_entry.set_age(current_age);
                (pk, one_element_slot_list_entry)
            })
            .collect();

        let (to_flush, to_evict) = InMemAccountsIndex::<u64, u64>::gather_possible_evictions(
            map.iter(),
            current_age,
            ages_flushing_now,
            max_evictions,
        );

        let total_selected = to_flush.0.len() + to_evict.0.len();
        assert_eq!(total_selected, max_evictions.get());

        for key in to_flush.0.iter().chain(&to_evict.0) {
            let entry = map.get(key).unwrap();
            assert_eq!(entry.ref_count(), ref_count);
            assert!(InMemAccountsIndex::<u64, u64>::should_evict_based_on_age(
                current_age,
                entry,
                ages_flushing_now,
            ));
        }

        for key in &to_flush.0 {
            assert!(map.get(key).unwrap().dirty());
        }
        for key in &to_evict.0 {
            assert!(!map.get(key).unwrap().dirty());
        }
    }

    #[test]
    fn test_try_make_entry_for_flush() {
        let bucket = new_for_test::<u64>();

        // test: entry is ready for flush
        {
            let slot = 11;
            let account_info = 22_u64; // <-- u64 means *not* cached
            let entry = AccountMapEntry::new(
                SlotList::from_iter([(slot, account_info)]),
                /*ref count*/ 1,
                AccountMapEntryMeta::default(),
            );
            entry.set_dirty(true);

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 0, 0);
            assert_eq!(entry_for_flush, ShouldFlush::Yes((slot, account_info)));
        }

        // test: do not flush because not dirty
        {
            let entry = AccountMapEntry::new(
                SlotList::from_iter([(0, 0u64)]), // <-- u64 means *not* cached
                /*ref count*/ 1,
                AccountMapEntryMeta::default(),
            );
            entry.set_dirty(false);

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 0, 0);
            assert_eq!(entry_for_flush, ShouldFlush::No(ReasonToNotFlush::Clean),);
        }

        // test: do not flush due to age
        {
            let entry = AccountMapEntry::new(
                SlotList::from_iter([(0, 0u64)]), // <-- u64 means *not* cached
                /*ref count*/ 1,
                AccountMapEntryMeta::default(),
            );

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 1, 0);
            assert_eq!(entry_for_flush, ShouldFlush::No(ReasonToNotFlush::Age),);
        }

        // test: do not flush due to ref count
        {
            let entry = AccountMapEntry::new(
                SlotList::from_iter([(0, 0u64)]), // <-- u64 means *not* cached
                /*ref count*/ 2,
                AccountMapEntryMeta::default(),
            );
            entry.set_dirty(true);

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 0, 0);
            assert_eq!(entry_for_flush, ShouldFlush::No(ReasonToNotFlush::RefCount),);
        }

        // test: do not flush due to slot list len, part 1
        {
            let entry = AccountMapEntry::new(
                SlotList::new(), // <-- slot list is empty
                /*ref count*/ 1,
                AccountMapEntryMeta::default(),
            );
            entry.set_dirty(true);

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 0, 0);
            assert_eq!(
                entry_for_flush,
                ShouldFlush::No(ReasonToNotFlush::SlotListLen),
            );
        }

        // test: do not flush due to slot list len, part 2
        {
            let entry = AccountMapEntry::new(
                SlotList::from_iter([(0, 0u64), (1, 1)]), // <-- slot list has more than 1 item
                /*ref count*/ 1,
                AccountMapEntryMeta::default(),
            );
            entry.set_dirty(true);

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 0, 0);
            assert_eq!(
                entry_for_flush,
                ShouldFlush::No(ReasonToNotFlush::SlotListLen),
            );
        }

        // test: do not flush due to slot list cached
        {
            let bucket = new_for_test::<f64>();
            let entry = AccountMapEntry::new(
                SlotList::from_iter([(0, 0f64)]), // <-- f64 acts as cached
                /*ref count*/ 1,
                AccountMapEntryMeta::default(),
            );
            entry.set_dirty(true);

            let entry_for_flush = bucket.try_make_entry_for_flush(&entry, 0, 0);
            assert_eq!(
                entry_for_flush,
                ShouldFlush::No(ReasonToNotFlush::SlotListCached),
            );
        }
    }

    #[test]
    fn test_age() {
        agave_logger::setup();
        let test = new_for_test::<u64>();
        assert!(test.get_should_age(test.storage.current_age()));
        assert_eq!(test.storage.count_buckets_flushed(), 0);
        test.set_has_aged(0, true);
        assert!(!test.get_should_age(test.storage.current_age()));
        assert_eq!(test.storage.count_buckets_flushed(), 1);
        // simulate rest of buckets aging
        for _ in 1..BINS_FOR_TESTING {
            assert!(!test.storage.all_buckets_flushed_at_current_age());
            test.storage.bucket_flushed_at_current_age(true);
        }
        assert!(test.storage.all_buckets_flushed_at_current_age());
        // advance age
        test.storage.increment_age();
        assert_eq!(test.storage.current_age(), 1);
        assert!(!test.storage.all_buckets_flushed_at_current_age());
        assert!(test.get_should_age(test.storage.current_age()));
        assert_eq!(test.storage.count_buckets_flushed(), 0);
    }

    #[test]
    fn test_update_slot_list_other_reclaim_old_slots() {
        agave_logger::setup();
        let reclaim = UpsertReclaim::ReclaimOldSlots;
        let new_slot = 5;
        let info = 1;
        let other_value = info + 1;
        let at_new_slot = (new_slot, info);
        let unique_other_slot = new_slot + 1;
        for other_slot in [Some(new_slot), Some(unique_other_slot), None] {
            let mut reclaims = ReclaimsSlotList::new();
            let entry = AccountMapEntry::empty_for_tests();
            let mut slot_list = entry.slot_list_write_lock();
            // upserting into empty slot_list, so always addref
            assert_eq!(
                InMemAccountsIndex::<u64, u64>::update_slot_list(
                    &mut slot_list,
                    new_slot,
                    info,
                    other_slot,
                    &mut reclaims,
                    reclaim
                ),
                (1, 1),
                "other_slot: {other_slot:?}"
            );
            assert_eq!(slot_list.clone_list(), SlotList::from([at_new_slot]));
            assert!(reclaims.is_empty());
        }

        // replace other
        let entry = AccountMapEntry::new(
            SlotList::from([(unique_other_slot, other_value)]),
            1,
            AccountMapEntryMeta::default(),
        );
        let mut slot_list = entry.slot_list_write_lock();
        let expected_reclaims = ReclaimsSlotList::from(slot_list.as_ref());
        let other_slot = Some(unique_other_slot);
        let mut reclaims = ReclaimsSlotList::new();
        assert_eq!(
            // upserting into slot_list that does NOT contain an entry at 'new_slot'
            // but, it DOES contain an entry at other_slot, so we do NOT add-ref. The assumption is that 'other_slot' is going away
            // and that the previously held add-ref is now used by 'new_slot'
            InMemAccountsIndex::<u64, u64>::update_slot_list(
                &mut slot_list,
                new_slot,
                info,
                other_slot,
                &mut reclaims,
                reclaim
            ),
            (0, 1),
            "other_slot: {other_slot:?}"
        );
        assert_eq!(slot_list.clone_list(), SlotList::from([at_new_slot]));
        assert_eq!(reclaims, expected_reclaims);

        // nothing will exist at this slot
        let missing_other_slot = unique_other_slot + 1;
        let ignored_slot = 10; // bigger than is used elsewhere in the test
        let ignored_value = info + 10;
        let reclaimed_slot = 1; // less than is used elsewhere in the test
        let reclaimed_value = info + 10;

        // build a list of possible contents in the slot_list prior to calling 'update_slot_list'
        let possible_initial_slot_list_contents = {
            let mut possible_initial_slot_list_contents = Vec::new();

            // Add ignored slot account_info entries (slots with larger slot #s than 'new_slot' or 'other_slot')
            possible_initial_slot_list_contents
                .extend((0..3).map(|i| (ignored_slot + i, ignored_value + i)));

            // Add reclaimed slot account_info entries (slots with smaller slot #s than 'new_slot' or 'other_slot')
            possible_initial_slot_list_contents
                .extend((0..3).map(|i| (reclaimed_slot + i, reclaimed_value + i)));

            // Add account_info for 'new_slot'
            possible_initial_slot_list_contents.push(at_new_slot);
            // Add account_info for 'other_slot'
            possible_initial_slot_list_contents.push((unique_other_slot, other_value));
            possible_initial_slot_list_contents
        };

        /*
         * loop over all possible permutations of 'possible_initial_slot_list_contents'
         * some examples:
         * []
         * [other]
         * [other, new_slot]
         * [new_slot, other]
         * [dummy0, new_slot, dummy1, other] (and all permutation of this order)
         * [other, dummy1, new_slot] (and all permutation of this order)
         * ...
         * [dummy0, new_slot, dummy1, other_slot, dummy2] (and all permutation of this order)
         */
        let mut attempts = 0;
        // loop over each initial size of 'slot_list'
        for initial_slot_list_len in 0..=possible_initial_slot_list_contents.len() {
            // loop over every permutation of possible_initial_slot_list_contents within a list of len 'initial_slot_list_len'
            for content_source_indexes in
                (0..possible_initial_slot_list_contents.len()).permutations(initial_slot_list_len)
            {
                // loop over each possible parameter for 'other_slot'
                for other_slot in [
                    Some(new_slot),
                    Some(unique_other_slot),
                    Some(missing_other_slot),
                    None,
                ] {
                    if other_slot.is_some()
                        && new_slot != other_slot.unwrap()
                        && slot_list.contains(&(new_slot, info))
                    {
                        // skip this permutation if 'new_slot' is already in the slot_list, but we are trying to reclaim other slot
                        // This is an assert case as only one of new_slot and other_slot should be in the slot list
                        continue;
                    }

                    attempts += 1;
                    // initialize slot_list prior to call to 'InMemAccountsIndex::update_slot_list'
                    // by inserting each possible entry at each possible position
                    let entry = AccountMapEntry::new(
                        content_source_indexes
                            .iter()
                            .map(|i| possible_initial_slot_list_contents[*i])
                            .collect(),
                        1,
                        AccountMapEntryMeta::default(),
                    );
                    let mut slot_list = entry.slot_list_write_lock();
                    let mut expected = slot_list.clone_list();
                    let original = slot_list.clone_list();
                    let mut reclaims = ReclaimsSlotList::new();

                    let (result, _len) = InMemAccountsIndex::<u64, u64>::update_slot_list(
                        &mut slot_list,
                        new_slot,
                        info,
                        other_slot,
                        &mut reclaims,
                        reclaim,
                    );
                    let mut slot_list = slot_list.clone_list();

                    // calculate expected reclaims
                    let mut expected_reclaims = ReclaimsSlotList::new();
                    expected.retain(|(slot, info)| {
                        let retain = *slot > new_slot;
                        if !retain {
                            expected_reclaims.push((*slot, *info));
                        }
                        retain
                    });
                    expected.push((new_slot, info));

                    // Calculate the expected ref count change. It is expected to be 1 - the number of reclaims
                    let expected_result = 1 - expected_reclaims.len() as i32;
                    assert_eq!(
                        expected_result, result,
                        "return value different. other: {other_slot:?}, {expected:?}, \
                         {slot_list:?}, original: {original:?}"
                    );
                    // sort for easy comparison
                    expected_reclaims.sort_unstable();
                    reclaims.sort_unstable();
                    assert_eq!(
                        expected_reclaims, reclaims,
                        "reclaims different. other: {other_slot:?}, {expected:?}, {slot_list:?}, \
                         original: {original:?}"
                    );
                    // sort for easy comparison
                    slot_list.sort_unstable();
                    expected.sort_unstable();
                    assert_eq!(
                        slot_list, expected,
                        "slot_list different. other: {other_slot:?}, {expected:?}, {slot_list:?}, \
                         original: {original:?}"
                    );
                }
            }
        }
        assert_eq!(attempts, 219202); // complicated permutations, so make sure we ran the right #
    }

    #[should_panic(expected = "slot_list has slot in slot_list but is not replacing it")]
    #[test_case(2; "Slot to replace is in the slot list")]
    #[test_case(3; "Slot to replace is not in the slot list")]
    fn test_update_slot_list_new_slot_duplicate_panic(slot_to_replace: u64) {
        let new_slot = 1; // This slot already exists in the list
        let old_slot = 2; // This slot already exists in the list
        let entry = AccountMapEntry::new(
            SlotList::from_iter([(new_slot, 0u64), (old_slot, 0)]),
            1,
            AccountMapEntryMeta::default(),
        );
        let mut slot_list = entry.slot_list_write_lock();
        let mut reclaims = ReclaimsSlotList::new();
        let new_info = 1;

        // Attempt to update the slot list with a duplicate slot, which should trigger the panic
        InMemAccountsIndex::<u64, u64>::update_slot_list(
            &mut slot_list,
            new_slot,
            new_info,
            Some(slot_to_replace),
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
    }

    #[test]
    fn test_flush_guard() {
        let flushing_active = AtomicBool::new(false);

        {
            let flush_guard = FlushGuard::lock(&flushing_active);
            assert!(flush_guard.is_some());
            assert!(flushing_active.load(Ordering::Acquire));

            {
                // Trying to lock the FlushGuard again will not succeed.
                let flush_guard2 = FlushGuard::lock(&flushing_active);
                assert!(flush_guard2.is_none());
            }

            // The `flushing_active` flag will remain true, even after `flush_guard2` goes out of
            // scope (and is dropped).  This ensures `lock()` and `drop()` work harmoniously.
            assert!(flushing_active.load(Ordering::Acquire));
        }

        // After the FlushGuard is dropped, the flag will be cleared.
        assert!(!flushing_active.load(Ordering::Acquire));
    }

    #[test]
    fn test_remove_if_slot_list_empty_entry() {
        let key = solana_pubkey::new_rand();
        let unknown_key = solana_pubkey::new_rand();

        let test = new_for_test::<u64>();

        let mut map = test.map_internal.write().unwrap();

        {
            // item is NOT in index at all, still return true from remove_if_slot_list_empty_entry
            // make sure not initially in index
            let entry = map.entry(unknown_key);
            assert_matches!(entry, Entry::Vacant(_));
            let entry = map.entry(unknown_key);
            assert!(test.remove_if_slot_list_empty_entry(entry));
            // make sure still not in index
            let entry = map.entry(unknown_key);
            assert_matches!(entry, Entry::Vacant(_));
        }

        {
            // add an entry with an empty slot list
            let val = Box::new(AccountMapEntry::<u64>::empty_for_tests());
            map.insert(key, val);
            let entry = map.entry(key);
            assert_matches!(entry, Entry::Occupied(_));
            // should have removed it since it had an empty slot list
            assert!(test.remove_if_slot_list_empty_entry(entry));
            let entry = map.entry(key);
            assert_matches!(entry, Entry::Vacant(_));
            // return true - item is not in index at all now
            assert!(test.remove_if_slot_list_empty_entry(entry));
        }

        {
            // add an entry with a NON empty slot list - it will NOT get removed
            let val = Box::new(AccountMapEntry::<u64>::empty_for_tests());
            val.slot_list_write_lock().push((1, 1));
            map.insert(key, val);
            // does NOT remove it since it has a non-empty slot list
            let entry = map.entry(key);
            assert!(!test.remove_if_slot_list_empty_entry(entry));
            let entry = map.entry(key);
            assert_matches!(entry, Entry::Occupied(_));
        }
    }

    #[test]
    fn test_lock_and_update_slot_list() {
        let test = AccountMapEntry::<u64>::empty_for_tests();
        let info = 65;
        let mut reclaims = ReclaimsSlotList::new();
        // first upsert, should increase
        let len = InMemAccountsIndex::<u64, u64>::lock_and_update_slot_list(
            &test,
            (1, info),
            None,
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
        assert_eq!(test.slot_list_lock_read_len(), len);
        assert_eq!(len, 1);
        // update to different slot, should increase
        let len = InMemAccountsIndex::<u64, u64>::lock_and_update_slot_list(
            &test,
            (2, info),
            None,
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
        assert_eq!(test.slot_list_lock_read_len(), len);
        assert_eq!(len, 2);
        // update to same slot, should not increase
        let len = InMemAccountsIndex::<u64, u64>::lock_and_update_slot_list(
            &test,
            (2, info),
            None,
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
        assert_eq!(test.slot_list_lock_read_len(), len);
        assert_eq!(len, 2);
    }

    #[test_case(Some(10000);  "with pre-allocation 10000")]
    #[test_case(Some(20000);  "with pre-allocation 20000")]
    #[test_case(Some(30000);  "with pre-allocation 30000")]
    #[test_case(None; "without pre-allocation")]
    fn test_new_with_num_initial_accounts(num_initial_accounts: Option<usize>) {
        let config = AccountsIndexConfig::default();

        let bin_counts = [2, 4, 8];

        for bin_count in bin_counts {
            let holder = Arc::new(BucketMapHolder::new(bin_count, &config, 1));
            let mut total_capacity = 0;

            for bin in 0..bin_count {
                let accounts_index =
                    InMemAccountsIndex::<u64, u64>::new(&holder, bin, num_initial_accounts);
                total_capacity += accounts_index.map_internal.read().unwrap().capacity();
            }

            if let Some(num_initial_accounts) = num_initial_accounts {
                assert!(total_capacity > num_initial_accounts);
            } else {
                assert_eq!(total_capacity, 0);
            }
        }
    }
}
