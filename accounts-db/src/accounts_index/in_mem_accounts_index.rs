use {
    super::{
        DiskIndexValue, IndexValue, ReclaimsSlotList, RefCount, SlotList, SlotListItem,
        UpsertReclaim,
        account_map_entry::{
            AccountMapEntry, AccountMapEntryMeta, PreAllocatedAccountMapEntry, SlotListWriteGuard,
        },
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        stats::Stats,
    },
    rand::{Rng, rng},
    solana_bucket_map::bucket_api::BucketApi,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    std::{
        cmp,
        collections::{HashMap, HashSet, hash_map::Entry},
        fmt::Debug,
        mem,
        num::NonZeroUsize,
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
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

    /// Whether to write through to disk on upsert (true when threshold-based bin management is active)
    should_write_through: bool,
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
            // should_write_through: write through on every upsert so inline eviction can fire immediately
            should_write_through: storage.threshold_entries_per_bin.is_some(),
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

    /// Convenience wrapper for slot_list_mut_with_entry that ignores the entry
    pub(crate) fn slot_list_mut<RT>(
        &self,
        pubkey: &Pubkey,
        user_fn: impl FnOnce(SlotListWriteGuard<T>) -> RT,
    ) -> Option<RT> {
        self.slot_list_mut_with_entry(pubkey, |slot_list, _entry| user_fn(slot_list))
    }

    /// Call `user_fn` with a write lock of the slot list and the entry itself.
    /// The entry is always marked dirty after `user_fn` returns, regardless of whether
    /// `user_fn` modifies the slot list — callers should ideally know they will modify it.
    /// When write-through is active and the resulting slot list has exactly one entry with
    /// ref_count=1, the entry is additionally flushed to disk immediately and the dirty
    /// flag may be cleared.
    pub(crate) fn slot_list_mut_with_entry<RT>(
        &self,
        pubkey: &Pubkey,
        user_fn: impl FnOnce(SlotListWriteGuard<T>, &AccountMapEntry<T>) -> RT,
    ) -> Option<RT> {
        let mut write_through_args: Option<(Slot, T)> = None;
        let result = self.get_internal_inner(pubkey, |entry| {
            (
                true,
                entry.map(|entry| {
                    let result = user_fn(entry.slot_list_write_lock(), entry);
                    // always mark dirty unconditionally, even if user_fn made no changes
                    entry.mark_dirty();
                    if self.should_write_through && entry.ref_count() == 1 {
                        let slot_list = entry.slot_list_read_lock();
                        if slot_list.len() == 1 {
                            write_through_args = Some(slot_list[0]);
                        }
                    }
                    result
                }),
            )
        });
        if let Some((slot, account_info)) = write_through_args {
            self.write_through(pubkey, slot, account_info);
        }
        result
    }

    /// Writes `disk_entry` for `pubkey` to `disk`, retrying after a grow if needed.
    /// Returns the total time spent waiting for disk grows, in microseconds.
    fn write_to_disk(
        disk: &BucketApi<(Slot, U)>,
        pubkey: &Pubkey,
        disk_entry: &[(Slot, U)],
    ) -> u64 {
        let mut grow_us = 0u64;
        loop {
            match disk.try_write(pubkey, (disk_entry, 1)) {
                Ok(_) => break,
                Err(err) => {
                    let m = Measure::start("flush_grow");
                    disk.grow(err);
                    grow_us += m.end_as_us();
                }
            }
        }
        grow_us
    }

    /// Write `(slot, account_info)` to the disk index, then under the slot list read lock
    /// verify the in-mem entry still matches; if so, clear the dirty flag so the entry
    /// is eligible for eviction without waiting for the background flush.
    ///
    /// We hold the slot list read lock during the equality check to prevent concurrent
    /// modifications from invalidating our check between the disk write and the dirty-clear.
    /// Any concurrent upsert that modifies the slot list must hold the write lock, so it
    /// cannot proceed until we release. If it ran before us the check will fail and we leave
    /// the entry dirty for the next write to clean up; if it runs after, it will re-dirty
    /// the now-clean entry and call write_through itself.
    fn write_through(&self, pubkey: &Pubkey, slot: Slot, account_info: T) {
        let disk = self.bucket.as_ref().unwrap();
        let disk_entry = [(slot, account_info.into())];
        let grow_us = Self::write_to_disk(disk, pubkey, &disk_entry);
        Self::update_stat(&self.stats().flush_entries_updated_on_disk_immediate, 1);
        Self::update_stat(&self.stats().flush_grow_us, grow_us);
        self.get_only_in_mem(pubkey, false, |entry| {
            if let Some(entry) = entry {
                let slot_list = entry.slot_list_read_lock();
                if slot_list.len() == 1
                    && slot_list[0] == (slot, account_info)
                    && entry.ref_count() == 1
                {
                    entry.clear_dirty();
                }
            }
        });
    }

    /// Clean the slot list by removing all slot_list items older than the max_slot.
    /// Decrease the reference count of the entry by the number of removed accounts.
    /// Note: This must only be called on startup, and reclaims must be reclaimed.
    pub(crate) fn clean_and_unref_slot_list_on_startup(
        &self,
        pubkey: &Pubkey,
        reclaims: &mut ReclaimsSlotList<T>,
    ) {
        self.slot_list_mut_with_entry(pubkey, |mut slot_list, entry| {
            let max_slot = slot_list
                .iter()
                .map(|(slot, _account)| *slot)
                .max()
                .expect("Slot list has entries");

            let mut reclaim_count = 0;
            let count = slot_list.retain_and_count(|(slot, value)| {
                // keep the newest entry, and reclaim all others
                if *slot < max_slot {
                    assert!(!value.is_cached(), "Unsafe to reclaim cached entries");
                    reclaims.push((*slot, *value));
                    reclaim_count += 1;
                    false
                } else {
                    true
                }
            });

            assert_eq!(
                count, 1,
                "Slot list should have exactly one entry after cleaning"
            );

            entry.unref_by_count(reclaim_count);
            assert_eq!(
                entry.ref_count(),
                1,
                "ref count should be one after cleaning all entries"
            );
        })
        .expect("Expected entry to exist in accounts index");
    }

    /// Insert a cached entry into the accounts index
    /// If the entry is already present, just mark dirty and set the age to the future
    fn cache_entry_at_slot(current: &AccountMapEntry<T>, new_value: SlotListItem<T>) {
        let mut slot_list = current.slot_list_write_lock();
        let (slot, new_entry) = new_value;
        // Find and replace existing entry at this slot, or append if not found
        if let Some(existing_entry) = slot_list.iter_mut().find(|(s, _)| *s == slot) {
            existing_entry.1 = new_entry;
        } else {
            slot_list.push((slot, new_entry));
        }
        current.mark_dirty();
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
        let mut should_write_through = false;

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
                should_write_through =
                    self.should_write_through && slot_list_length == 1 && entry.ref_count() == 1;
                self.set_age_to_future(entry, slot_list_length > 1);
            }
        });
        if should_write_through {
            self.write_through(pubkey, slot, account_info);
        }
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

                // Inline eviction: if at capacity and this pubkey is not already in the map,
                // evict one clean entry to make room before inserting.
                // Only enable when should_write_through is true, as finding a candidate for eviction
                // is expensive when the dirty entries are not being written through
                // This is a rare case; background eviction clears the excess over time.
                if self.should_write_through
                    && self.storage.should_evict_based_on_count(map.len())
                    && !map.contains_key(pubkey)
                {
                    let evict_key = map.iter().find(|(_, v)| !v.dirty()).map(|(k, _)| *k);
                    if let Some(key) = evict_key {
                        debug_assert!(
                            self.load_from_disk(&key).is_some(),
                            "inline eviction target must be on disk"
                        );
                        map.remove(&key);
                        stats.sub_mem_count(1);
                        Self::update_stat(&stats.flush_entries_evicted_from_mem_immediate, 1);
                    }
                }

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
        current.mark_dirty();
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
        // then we will mark_dirty() and skip the disk update. The dirty flag will ensure the
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
            entry.mark_dirty();
            return ShouldFlush::No(ReasonToNotFlush::RefCount);
        }

        if slot_list.len() != 1 {
            // we only flush regular entries, i.e. slot list len == 1
            entry.mark_dirty();
            return ShouldFlush::No(ReasonToNotFlush::SlotListLen);
        }

        // SAFETY: We just checked that the slot list len is 1
        let slot_list_elem = slot_list[0];

        if slot_list_elem.1.is_cached() {
            // we only flush regular entries, i.e. slot list does not contain any cached entries
            entry.mark_dirty();
            return ShouldFlush::No(ReasonToNotFlush::SlotListCached);
        }

        // entry is ready to be flushed
        ShouldFlush::Yes(slot_list_elem)
    }

    /// Collect candidates to flush/evict from `iter` by checking age
    /// Skip entries with ref_count != 1 since they will be rejected later anyway
    fn gather_possible_flush_evict_candidates<'a>(
        iter: impl Iterator<Item = (&'a Pubkey, &'a Box<AccountMapEntry<T>>)>,
        current_age: Age,
        ages_flushing_now: Age,
        max_evictions: NonZeroUsize,
        collect_flush_candidates: bool,
    ) -> (CandidatesToFlush, CandidatesToEvict) {
        let mut candidates_to_flush = Vec::new();
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

            if v.dirty() {
                if collect_flush_candidates {
                    candidates_to_flush.push(*k);
                }
            } else {
                sampling_state.select(*k, &mut rng);
            }
        }
        (
            CandidatesToFlush(candidates_to_flush),
            CandidatesToEvict(mem::take(&mut sampling_state.samples)),
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
            let max_evictions = self.storage.max_evictions_for_threshold(map.len());
            let possible_evictions = Self::gather_possible_flush_evict_candidates(
                map.iter(),
                current_age,
                ages_flushing_now,
                max_evictions,
                !self.should_write_through,
            );
            (possible_evictions, m)
        };
        Self::update_time_stat(&self.stats().flush_scan_us, m);

        possible_evictions
    }

    /// Takes self's `startup_info` and writes it to disk and in-mem.
    ///
    /// If the configured memory limit is "minimal", nothing is writen to in-mem.
    /// Otherwise write to in-mem (and respect the memory limit).
    fn write_startup_info(&self) {
        let insert = std::mem::take(&mut *self.startup_info.insert.lock().unwrap());
        if insert.is_empty() {
            // nothing to insert for this bin
            return;
        }

        // this fn should only be called from a single thread, so holding the lock is fine
        let mut duplicates = self.startup_info.duplicates.lock().unwrap();

        // merge all items into the disk index now
        let disk = self.bucket.as_ref().unwrap();
        let duplicate_entries_and_indices = disk.batch_insert_non_duplicates(&insert);
        let duplicate_addresses: HashSet<_> = duplicate_entries_and_indices
            .iter()
            .map(|(index, _entry)| &insert[*index].0)
            .collect();
        let mut count = insert.len() as u64;
        for (i, duplicate_entry) in duplicate_entries_and_indices {
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

        if let Some(threshold_entries_per_bin) = self.storage.threshold_entries_per_bin.as_ref() {
            // If a memory threshold is set, then insert into the in-mem index here,
            // up to that limit.  This way we pre-populate the in-mem index, and can
            // avoid having to load some entries from disk on first access.
            let mut map = self.map_internal.write().unwrap();
            // Insert up to the low water mark.  Purposely do not insert all the way up  to the
            // high water mark, as that then causes the flush loop condition to immediately trigger
            // and evict down to the low water mark anyway.
            let num_available = threshold_entries_per_bin
                .low_water_mark
                .saturating_sub(map.len());
            for (address, (slot, disk_index_value)) in insert
                .iter()
                .filter(|(address, _entry)| !duplicate_addresses.contains(address)) // <- skip known duplicates
                .take(num_available)
            {
                match map.entry(*address) {
                    Entry::Vacant(vacant) => {
                        let index_value = (*disk_index_value).into();
                        let slot_list = SlotList::from([(*slot, index_value)]);
                        let ref_count = 1;
                        let meta = AccountMapEntryMeta::new_clean(&self.storage);
                        let account_map_entry = AccountMapEntry::new(slot_list, ref_count, meta);
                        vacant.insert(Box::new(account_map_entry));
                    }
                    Entry::Occupied(_occupied) => {
                        // If the account already has an entry in the in-mem index, then that means
                        // it is a duplicate.  We could merge them here, however duplicates
                        // handling happens later during startup/index generation, in
                        // populate_and_retrieve_duplicate_keys_from_startup(), which will insert
                        // them back into the in-mem index.  Thus we should *not* insert any
                        // accounts with duplicate entries here.
                        // Additionally, once marking obsolete accounts is always on, we then
                        // should no longer have any duplicates to worry about.
                    }
                }
            }

            // Related to the comment in the Entry::Occupied match arm above, if inserting
            // into disk (batch_insert_non_duplicates()) returned duplicates, we need to check
            // and make sure they are not in the in-mem index.  (Since the first time we encounter
            // a duplicate we do not know it is a duplicate, so it will have been inserted
            // in mem.)  We must remove them here.
            for duplicate_address in duplicate_addresses {
                map.remove(duplicate_address);
            }
            drop(map);
        } else {
            // Else, we should not have anything in the in-mem index at all.
            let map_internal = self.map_internal.read().unwrap();
            assert!(
                map_internal.is_empty(),
                "len: {}, first: {:?}",
                map_internal.len(),
                map_internal.iter().take(1).collect::<Vec<_>>()
            );
            drop(map_internal);
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

    /// Decide whether this bin needs flushing/eviction. Returns true if the caller should proceed.
    ///
    /// Fires on either of two conditions:
    /// - Free-entry headroom is below the configured overhead. Tombstones left by prior evictions
    ///   reduce capacity without being included in len, so a rehash can be imminent before the
    ///   count crosses the high-water mark. This is the primary trigger in steady state.
    /// - Entry count exceeds the high-water mark. This is a backstop for the case where the
    ///   hashmap has already doubled in size, leaving plenty of headroom so the first condition
    ///   would not fire on its own.
    ///
    /// Returns false for bins still in initial growth (capacity below `high_water_mark`).
    fn check_flush_trigger(&self) -> bool {
        let (entries_in_bin, capacity) = {
            let map = self.map_internal.read().unwrap();
            (map.len(), map.capacity())
        };

        // Skip during initial growth: below HWM, low free entries reflect a not-yet-grown
        // table, not tombstones. If tombstones do force a doubling before len crosses HWM,
        // the count check catches it later once len grows past HWM.
        if let Some(thresholds) = &self.storage.threshold_entries_per_bin {
            if capacity < thresholds.high_water_mark {
                return false;
            }
        }

        let high_count_triggered = self.storage.should_evict_based_on_count(entries_in_bin);
        let low_free_entries_triggered = self
            .storage
            .should_evict_based_on_free_entries(capacity.saturating_sub(entries_in_bin));
        if !high_count_triggered && !low_free_entries_triggered {
            return false;
        }
        if low_free_entries_triggered {
            // Primary case: low free-entry headroom (typically from tombstones).
            Self::update_stat(&self.stats().evict_triggered_by_low_free_entries, 1);
        } else {
            // Backstop: bin is past the high-water mark while free-entry headroom
            // still has slack — typically because the hashmap doubled in size.
            Self::update_stat(&self.stats().evict_triggered_by_high_count, 1);
        }
        true
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
            self.write_startup_info();

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

        if !self.check_flush_trigger() {
            // Still mark as aged to avoid infinite scanning
            assert_eq!(current_age, self.storage.current_age());
            self.set_has_aged(current_age, can_advance_age);
            return;
        }

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
        for key in candidates_to_flush.0 {
            // Entry was dirty at scan time, need to write to disk
            let lock_measure = Measure::start("flush_read_lock");
            let map_read_guard = self.map_internal.read().unwrap();
            let Some(entry) = map_read_guard.get(&key) else {
                continue;
            };

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
                        ReasonToNotFlush::RefCount => flush_stats.num_not_flushed_ref_count += 1,
                        ReasonToNotFlush::SlotListLen => {
                            flush_stats.num_not_flushed_slot_list_len += 1
                        }
                        ReasonToNotFlush::SlotListCached => {
                            flush_stats.num_not_flushed_slot_list_cached += 1
                        }
                    }
                    continue;
                }
            };
            let disk_entry = [(slot, account_info.into())];

            // Now write to disk WITHOUT holding any locks
            flush_stats.flush_grow_us += Self::write_to_disk(disk, &key, &disk_entry);
            flush_stats.flush_entries_updated_on_disk_background += 1;
        }
        flush_stats.flush_update_us = flush_update_measure.end_as_us();
        flush_stats.update_to_stats(self.stats());

        let m = Measure::start("flush_evict");
        self.evict_from_cache(&candidates_to_evict.0, current_age, ages_flushing_now);
        Self::update_time_stat(&self.stats().flush_evict_us, m);

        if iterate_for_age {
            // completed iteration of the buckets at the current age
            assert_eq!(current_age, self.storage.current_age());
            self.set_has_aged(current_age, can_advance_age);
        }
    }

    /// Rebuild the bin's HashMap into a fresh allocation to clear tombstones left
    /// behind by evictions. hashbrown counts tombstones against `capacity`, so
    /// without this the bin's effective capacity drifts down over time and triggers
    /// the hashmap to double in capacity.
    ///
    /// Only called in Threshold mode, where `capacity >= target_entries` is guaranteed
    /// by the time eviction runs (`check_flush_trigger` gates on `high_water_mark`).
    fn reallocate_to_clear_tombstones(&self) {
        let stats = self.stats();
        let m = Measure::start("reallocate_hashmap");

        let target_entries = self
            .storage
            .threshold_entries_per_bin
            .as_ref()
            .expect("reallocate_to_clear_tombstones only runs in Threshold mode")
            .target_entries;

        let mut map = self.map_internal.write().unwrap();
        let capacity_pre = map.capacity();

        // Drain the old map into a fresh allocation sized to `target_entries` so the
        // backing storage stays stable across eviction cycles. Building a brand-new
        // map (rather than `shrink_to_fit`) guarantees a full rehash, which is what
        // actually clears the tombstones.
        let mut new_map = HashMap::with_capacity_and_hasher(target_entries, map.hasher().clone());
        new_map.extend(map.drain());
        *map = new_map;
        let capacity_post = map.capacity();
        drop(map);

        stats.update_in_mem_capacity(capacity_pre, capacity_post);
        Self::update_stat(&stats.num_hashmap_reallocates, 1);
        Self::update_time_stat(&stats.hashmap_reallocate_us, m);
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

        // Only Threshold mode cares about tombstone-driven capacity doublings; Minimal
        // evicts everything each pass, so rebuilding every flush is wasted work.
        if evicted > 0 && self.storage.threshold_entries_per_bin.is_some() {
            self.reallocate_to_clear_tombstones();
        }

        stats.sub_mem_count(evicted);
        Self::update_stat(
            &stats.flush_entries_evicted_from_mem_background,
            evicted as u64,
        );
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

    /// Returns the length and capacity of this bin's map
    ///
    /// Only intended to be called at startup, since it grabs the map's read lock.
    pub(crate) fn len_and_cap_for_startup(&self) -> (usize, usize) {
        let map = self.map_internal.read().unwrap();
        (map.len(), map.capacity())
    }

    /// Returns the number of entries currently held in memory for this bin.
    pub(crate) fn len(&self) -> usize {
        self.map_internal.read().unwrap().len()
    }
}

/// State of reservoir sampling algorithm for flush/eviction candidates.
#[derive(Debug)]
struct ReservoirState {
    samples: Vec<Pubkey>,
    seen: usize,
    max_samples: NonZeroUsize,
}

impl ReservoirState {
    /// Select a candidate, keeping a bounded roughly uniform sample set.
    fn select(&mut self, candidate: Pubkey, rng: &mut impl Rng) {
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
    /// Number of entries successfully written to disk by the background flush
    flush_entries_updated_on_disk_background: u64,
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
            &stats.flush_entries_updated_on_disk_background,
            self.flush_entries_updated_on_disk_background,
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
        crate::accounts_index::{
            ACCOUNTS_INDEX_CONFIG_FOR_TESTING, AccountsIndexConfig, BINS_FOR_TESTING, IndexLimit,
            IndexLimitThreshold, bucket_map_holder::ThresholdEntriesPerBin,
        },
        assert_matches::assert_matches,
        itertools::Itertools,
        std::iter,
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

    /// Creates an index with `should_write_through = true` and a live disk bucket.
    ///
    /// Pass `Some((high, low))` to override the computed per-bin threshold with explicit water-marks.
    /// Pass `None` to keep the byte-based default, which is effectively unlimited.
    fn new_should_write_through_for_test(
        threshold: Option<(usize, usize)>,
    ) -> InMemAccountsIndex<u64, u64> {
        let config = AccountsIndexConfig {
            index_limit: IndexLimit::Threshold(IndexLimitThreshold {
                num_bytes: 25_000_000_000,
                num_entries_overhead: 1,
                num_entries_to_evict: 1,
            }),
            ..ACCOUNTS_INDEX_CONFIG_FOR_TESTING
        };
        let mut holder = BucketMapHolder::new(BINS_FOR_TESTING, &config, 1);
        if let Some((high_water_mark, low_water_mark)) = threshold {
            holder.threshold_entries_per_bin = Some(ThresholdEntriesPerBin {
                target_entries: high_water_mark + 1,
                high_water_mark,
                low_water_mark,
            });
        }
        let holder = Arc::new(holder);
        let index = InMemAccountsIndex::<u64, u64>::new(&holder, 0, None);
        assert!(index.should_write_through);
        assert!(index.bucket.is_some());
        index
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

    /// Populates `index` with four entries covering the age/dirty matrix, triggers a
    /// background flush, then asserts the outcomes common to all flush modes:
    ///   - clean new: stays in memory, not on disk
    ///   - clean old: evicted from memory, not on disk
    ///   - dirty new: stays in memory, not on disk
    ///
    /// Returns `(pubkey_dirty_old, slot + 3, info + 3)` so the caller can assert the
    /// mode-specific outcome for the old dirty entry (flushed vs. skipped).
    fn flush_age_mixed_entries(
        accounts_index: &InMemAccountsIndex<u64, u64>,
    ) -> (Pubkey, Slot, u64) {
        let pubkey_clean_new = solana_pubkey::new_rand();
        let pubkey_clean_old = solana_pubkey::new_rand();
        let pubkey_dirty_new = solana_pubkey::new_rand();
        let pubkey_dirty_old = solana_pubkey::new_rand();
        let slot = 123;
        let info = 42;

        assert!(accounts_index.load_from_disk(&pubkey_clean_new).is_none());
        assert!(accounts_index.load_from_disk(&pubkey_clean_old).is_none());
        assert!(accounts_index.load_from_disk(&pubkey_dirty_new).is_none());
        assert!(accounts_index.load_from_disk(&pubkey_dirty_old).is_none());

        // A clean entry that is *not* in the flush/eviction window.
        // This entry should *not* be eligible for eviction.
        let entry_clean_new = AccountMapEntry::new(
            SlotList::from([(slot, info)]),
            1,
            AccountMapEntryMeta::new_clean(&accounts_index.storage),
        );
        assert!(!entry_clean_new.dirty());

        // A clean entry that *is* in the flush/eviction window.
        // This entry *should* be eligible for eviction.
        let entry_clean_old = AccountMapEntry::new(
            SlotList::from([(slot + 1, info + 1)]),
            1,
            AccountMapEntryMeta::new_clean(&accounts_index.storage),
        );
        entry_clean_old.set_age(accounts_index.storage.current_age());
        assert!(!entry_clean_old.dirty());

        // A dirty entry that is *not* in the flush/eviction window.
        // This entry should *not* be eligible for flush.
        let entry_dirty_new = AccountMapEntry::new(
            SlotList::from([(slot + 2, info + 2)]),
            1,
            AccountMapEntryMeta::new_dirty(&accounts_index.storage, false),
        );
        assert!(entry_dirty_new.dirty());

        // A dirty entry that *is* in the flush/eviction window.
        // This entry *should* be eligible for flush.
        let entry_dirty_old = AccountMapEntry::new(
            SlotList::from([(slot + 3, info + 3)]),
            1,
            AccountMapEntryMeta::new_dirty(&accounts_index.storage, false),
        );
        entry_dirty_old.set_age(accounts_index.storage.current_age());
        assert!(entry_dirty_old.dirty());

        accounts_index.map_internal.write().unwrap().extend([
            (pubkey_clean_new, Box::new(entry_clean_new)),
            (pubkey_clean_old, Box::new(entry_clean_old)),
            (pubkey_dirty_new, Box::new(entry_dirty_new)),
            (pubkey_dirty_old, Box::new(entry_dirty_old)),
        ]);

        accounts_index
            .remaining_ages_to_skip_flushing
            .store(0, Ordering::Release);

        accounts_index.flush(false);

        // clean new entry should not be flushed/evicted
        let mut found_in_mem = None;
        accounts_index.get_only_in_mem(&pubkey_clean_new, false, |entry| {
            found_in_mem = Some(entry.is_some());
            let entry = entry.expect("entry should remain in memory");
            assert!(!entry.dirty());
        });
        assert_eq!(found_in_mem, Some(true));
        assert!(accounts_index.load_from_disk(&pubkey_clean_new).is_none());

        // clean old entry should be evicted, and not flushed
        let mut found_in_mem = None;
        accounts_index.get_only_in_mem(&pubkey_clean_old, false, |entry| {
            found_in_mem = Some(entry.is_some());
        });
        assert_eq!(found_in_mem, Some(false));
        assert!(accounts_index.load_from_disk(&pubkey_clean_old).is_none());

        // dirty new entry should not be flushed/evicted
        let mut found_in_mem = None;
        accounts_index.get_only_in_mem(&pubkey_dirty_new, false, |entry| {
            found_in_mem = Some(entry.is_some());
            let entry = entry.expect("entry should remain in memory");
            assert!(entry.dirty());
        });
        assert_eq!(found_in_mem, Some(true));
        assert!(accounts_index.load_from_disk(&pubkey_dirty_new).is_none());

        (pubkey_dirty_old, slot + 3, info + 3)
    }

    #[test]
    fn test_flush_internal() {
        let accounts_index = new_disk_buckets_for_test::<u64>();
        let (pubkey_dirty_old, dirty_slot, dirty_info) = flush_age_mixed_entries(&accounts_index);

        // old dirty entry should be flushed, and not evicted
        let mut found_in_mem = None;
        accounts_index.get_only_in_mem(&pubkey_dirty_old, false, |entry| {
            found_in_mem = Some(entry.is_some());
            let entry = entry.expect("entry should remain in memory");
            assert!(!entry.dirty()); // flushing makes the entry clean

            // also ensure that old dirty entry can be evicted next time
            assert!(InMemAccountsIndex::<u64, u64>::should_evict_based_on_age(
                accounts_index.storage.current_age(),
                entry,
                accounts_index.num_ages_to_distribute_flushes,
            ));
        });
        assert_eq!(found_in_mem, Some(true));
        let (slot_list, ref_count) = accounts_index
            .load_from_disk(&pubkey_dirty_old)
            .expect("entry should be written to disk");
        assert_eq!(slot_list, SlotList::from([(dirty_slot, dirty_info)]));
        assert_eq!(ref_count, 1);
    }

    /// With `should_write_through=true`, the background flush should evict old clean entries but
    /// must NOT flush dirty entries to disk — dirty entries are written through inline on upsert.
    #[test]
    fn test_flush_internal_evicts_in_should_write_through_mode() {
        // high_water_mark=2: 4 entries puts us above threshold, triggering background eviction.
        let accounts_index = new_should_write_through_for_test(Some((2, 1)));
        let (pubkey_dirty_old, _, _) = flush_age_mixed_entries(&accounts_index);

        // old dirty entry should not be flushed, and not evicted
        let mut found_in_mem = None;
        accounts_index.get_only_in_mem(&pubkey_dirty_old, false, |entry| {
            found_in_mem = Some(entry.is_some());
            let entry = entry.expect("entry should remain in memory");
            assert!(entry.dirty()); // should_write_through: dirty entries are skipped by background flush
        });
        assert_eq!(found_in_mem, Some(true));
        assert!(accounts_index.load_from_disk(&pubkey_dirty_old).is_none());
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
    fn test_gather_possible_flush_evict_candidates() {
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
                entry.mark_dirty();
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
                assert!(!entry.dirty());
                entry.set_age(age);
                (Pubkey::new_unique(), entry)
            })
            .collect();

        for current_age in 0..=AGE_MAX {
            for ages_flushing_now in 0..=AGE_MAX {
                let (candidates_to_flush, candidates_to_evict) =
                    InMemAccountsIndex::<u64, u64>::gather_possible_flush_evict_candidates(
                        map_dirty.iter().chain(&map_clean),
                        current_age,
                        ages_flushing_now,
                        NonZeroUsize::new(map_dirty.len() + map_clean.len()).unwrap(),
                        true,
                    );
                // Verify that the number of entries selected for eviction matches the expected count.
                // Test setup: map contains 256 dirty entries and 256 clean entries.
                // Each with ages 0-255 (one entry per age value).
                //
                // gather_possible_flush_evict_candidates includes entries where:
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
    fn test_gather_possible_flush_and_evict_candidates_with_max_evictions() {
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
                    one_element_slot_list_entry.mark_dirty();
                }
                one_element_slot_list_entry.set_age(current_age);
                (pk, one_element_slot_list_entry)
            })
            .collect();

        let (to_flush, to_evict) =
            InMemAccountsIndex::<u64, u64>::gather_possible_flush_evict_candidates(
                map.iter(),
                current_age,
                ages_flushing_now,
                max_evictions,
                true,
            );

        assert_eq!(to_flush.0.len(), 128);
        assert_eq!(to_evict.0.len(), max_evictions.get());

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

    /// With `collect_flush_candidates=false`, dirty entries should not appear in either
    /// output list — not as flush candidates, and not as eviction candidates.
    #[test]
    fn test_gather_possible_flush_evict_candidates_no_flush() {
        let accounts_index = new_disk_buckets_for_test::<u64>();
        let current_age = accounts_index.storage.current_age();
        let ages_flushing_now = accounts_index.num_ages_to_distribute_flushes;
        let slot = 1;

        // Clean entry in the eviction window.
        let pubkey_clean = solana_pubkey::new_rand();
        let entry_clean = Box::new(AccountMapEntry::new(
            SlotList::from([(slot, 1)]),
            1,
            AccountMapEntryMeta::new_clean(&accounts_index.storage),
        ));
        entry_clean.set_age(current_age);

        // Dirty entry in the eviction window.
        let pubkey_dirty = solana_pubkey::new_rand();
        let entry_dirty = Box::new(AccountMapEntry::new(
            SlotList::from([(slot + 1, 2)]),
            1,
            AccountMapEntryMeta::new_dirty(&accounts_index.storage, false),
        ));
        entry_dirty.set_age(current_age);

        let map: HashMap<Pubkey, Box<AccountMapEntry<u64>>> =
            HashMap::from([(pubkey_clean, entry_clean), (pubkey_dirty, entry_dirty)]);

        let max_evictions = NonZeroUsize::new(map.len()).unwrap();
        let (to_flush, to_evict) =
            InMemAccountsIndex::<u64, u64>::gather_possible_flush_evict_candidates(
                map.iter(),
                current_age,
                ages_flushing_now,
                max_evictions,
                false, // should_write_through mode: do not collect flush candidates
            );

        assert!(to_flush.0.is_empty());
        assert!(!to_evict.0.contains(&pubkey_dirty));
        assert!(to_evict.0.contains(&pubkey_clean));
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
            entry.mark_dirty();

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
            assert!(!entry.dirty());

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
            entry.mark_dirty();

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
            entry.mark_dirty();

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
            entry.mark_dirty();

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
            entry.mark_dirty();

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

    /// Ensure `write_startup_info()` populates the in-mem index,
    /// while also respecting the configured memory threshold.
    #[test]
    fn test_write_startup_info() {
        let num_bins = 1;
        let num_entries_overhead = 300;
        let num_entries_to_evict = 200;
        let config = AccountsIndexConfig {
            bins: Some(num_bins),
            index_limit: {
                // Ensure we use an IndexLimit that (1) enables the disk index,
                // and (2) is a valid threshold, as per the logic in BucketMapHolder::new().
                // We will override the threshold afterwards, so the actual value doesn't matter.
                IndexLimit::Threshold(IndexLimitThreshold {
                    num_bytes: 25_000_000_000,
                    num_entries_overhead,
                    num_entries_to_evict,
                })
            },
            ..ACCOUNTS_INDEX_CONFIG_FOR_TESTING
        };
        let mut holder = BucketMapHolder::new(num_bins, &config, 1);

        // Override the threshold values to make testing faster.
        let low_water_mark = 100;
        let high_water_mark = low_water_mark + num_entries_to_evict;
        holder.threshold_entries_per_bin = Some(ThresholdEntriesPerBin {
            target_entries: high_water_mark + num_entries_overhead,
            high_water_mark,
            low_water_mark,
        });
        let holder = Arc::new(holder);
        let index = InMemAccountsIndex::<u64, u64>::new(&holder, num_bins - 1, None);

        // Emulate index generation where we push startup values into the `startup_info`
        // side-band struct when disk index is enabled.  Ensure we push more than
        // `low_water_mark` number of values.
        let to_insert = iter::repeat_with(|| {
            // the addresses need to be unique, but the actual values do not matter
            (Pubkey::new_unique(), (/*slot*/ 11, /*T*/ 42))
        })
        .take(high_water_mark);
        index.startup_info.insert.lock().unwrap().extend(to_insert);

        // Also push some duplicates, to ensure we do not put those in-mem
        let duplicate_pubkey = Pubkey::new_unique();
        {
            let mut startup_info_insert = index.startup_info.insert.lock().unwrap();
            // Yes, we want three duplicates.  Two is the minimum (by definition), but we want
            // three to ensure we don't see the first two, remove 'em, then see a third and think
            // "oh, this is a new non-duplicate!" and erroneously insert it in-mem.
            startup_info_insert.push((duplicate_pubkey, (/*slot*/ 13, /*T*/ 43)));
            startup_info_insert.push((duplicate_pubkey, (/*slot*/ 14, /*T*/ 44)));
            startup_info_insert.push((duplicate_pubkey, (/*slot*/ 15, /*T*/ 45)));
            // Reverse the vec to ensure the duplicates end up at the front.
            // Otherwise they would not be selected to be put in-mem.
            startup_info_insert.reverse();
        }
        assert!(index.map_internal.read().unwrap().is_empty());

        // Index generation calls `write_startup_info()`, which is responsible for writing the
        // values to disk, and also populating the in-mem index. So call `write_startup_info()`
        // here, and ensure:
        // - we end up with the expected number of items in the in-mem index
        // - duplicates do not end up in-mem
        index.write_startup_info();
        assert_eq!(index.map_internal.read().unwrap().len(), low_water_mark);
        assert!(
            !index
                .map_internal
                .read()
                .unwrap()
                .contains_key(&duplicate_pubkey)
        );
    }

    /// `slot_list_mut` write-through fires only for single-slot, ref_count=1 entries.
    #[test_case(SlotList::from([(1, 0)]), 1, true  ; "writes_through")]
    #[test_case(SlotList::from(vec![(1, 10), (2, 20)]),  1, false ; "multi_slot")]
    #[test_case(SlotList::from([(1, 0)]), 2, false ; "multi_ref")]
    fn test_slot_list_mut_write_through(
        slot_list: SlotList<u64>,
        ref_count: u32,
        expect_write_through: bool,
    ) {
        let index = new_should_write_through_for_test(None);
        let pubkey = solana_pubkey::new_rand();
        let entry = Box::new(AccountMapEntry::new(
            slot_list,
            ref_count,
            AccountMapEntryMeta::new_dirty(&index.storage, false),
        ));
        index.map_internal.write().unwrap().insert(pubkey, entry);
        index.slot_list_mut(&pubkey, |mut slot_list| {
            slot_list[0].1 = 2;
        });

        index.get_only_in_mem(&pubkey, false, |entry| {
            let entry = entry.expect("entry should be in memory");
            assert_eq!(!entry.dirty(), expect_write_through);
        });

        // Verify whether entry was flushed to disk or not
        assert_eq!(
            index.load_from_disk(&pubkey).is_some(),
            expect_write_through
        );
    }

    /// `upsert` with `should_write_through=true` writes through to disk and clears the dirty flag.
    #[test]
    fn test_upsert_write_through_clears_dirty() {
        let index = new_should_write_through_for_test(None);
        let pubkey = solana_pubkey::new_rand();
        let slot = 1;
        let info = 10;

        assert!(index.load_from_disk(&pubkey).is_none(), "not on disk yet");

        let new_value = PreAllocatedAccountMapEntry::new(slot, info, &index.storage, true);
        index.upsert(
            &pubkey,
            new_value,
            None,
            &mut ReclaimsSlotList::new(),
            UpsertReclaim::IgnoreReclaims,
        );

        index.get_only_in_mem(&pubkey, false, |entry| {
            let entry = entry.expect("entry should be in memory");
            assert!(!entry.dirty()); // write-through clears dirty
        });

        let (slot_list, ref_count) = index
            .load_from_disk(&pubkey)
            .expect("upsert should have written entry to disk");
        assert_eq!(slot_list, SlotList::from([(slot, info)]));
        assert_eq!(ref_count, 1);
    }

    /// When the bin exceeds the threshold and a new pubkey is inserted in `should_write_through`
    /// mode, one clean entry should be evicted inline to make room.
    #[test]
    fn test_inline_eviction_when_bin_exceeds_threshold() {
        // high_water_mark=2: after 3 insertions we are above threshold.
        let index = new_should_write_through_for_test(Some((2, 1)));
        let slot = 1;
        let info = 2;

        // Insert 3 entries via upsert — write-through will clean all of them.
        let initial_pubkeys: Vec<_> = (0..3).map(|_| solana_pubkey::new_rand()).collect();
        for pubkey in &initial_pubkeys {
            let new_value = PreAllocatedAccountMapEntry::new(slot, info, &index.storage, true);
            index.upsert(
                pubkey,
                new_value,
                None,
                &mut ReclaimsSlotList::new(),
                UpsertReclaim::IgnoreReclaims,
            );
        }
        assert_eq!(index.map_internal.read().unwrap().len(), 3);

        // Confirm all entries are clean (write-through fired) and present on disk.
        for pubkey in &initial_pubkeys {
            index.get_only_in_mem(pubkey, false, |entry| {
                let entry = entry.expect("entry should be in memory");
                assert!(!entry.dirty());
            });
            assert!(
                index.load_from_disk(pubkey).is_some(),
                "entry should be on disk after write-through upsert"
            );
        }

        // Insert a 4th new pubkey, this should lead to eviction
        let new_pubkey = solana_pubkey::new_rand();
        let new_value = PreAllocatedAccountMapEntry::new(slot, info + 1, &index.storage, true);
        index.upsert(
            &new_pubkey,
            new_value,
            None,
            &mut ReclaimsSlotList::new(),
            UpsertReclaim::IgnoreReclaims,
        );

        // Inline eviction removes one entry before inserting the new one, leaving the bin count at 3
        assert_eq!(index.map_internal.read().unwrap().len(), 3);

        // The new pubkey must be present in memory.
        let mut found = None;
        index.get_only_in_mem(&new_pubkey, false, |entry| found = Some(entry.is_some()));
        assert_eq!(
            found,
            Some(true),
            "newly inserted entry should be in memory"
        );

        // Exactly one of the original entries was evicted from memory (but remains on disk).
        let evicted_count = initial_pubkeys
            .iter()
            .filter(|pubkey| {
                let mut in_mem = false;
                index.get_only_in_mem(pubkey, false, |entry| in_mem = entry.is_some());
                !in_mem
            })
            .count();
        assert_eq!(
            evicted_count, 1,
            "exactly one original entry should have been evicted"
        );

        // The evicted entry should still be on disk (it was written through before eviction).
        let evicted_pubkey = initial_pubkeys
            .iter()
            .find(|pubkey| {
                let mut in_mem = false;
                index.get_only_in_mem(pubkey, false, |entry| in_mem = entry.is_some());
                !in_mem
            })
            .unwrap();
        assert!(
            index.load_from_disk(evicted_pubkey).is_some(),
            "evicted entry should still be on disk"
        );
    }

    /// While the bin's hashmap is still in initial growth (capacity below `high_water_mark`),
    /// the growth gate short-circuits check_flush_trigger to false even when the
    /// low-free-entries check would otherwise fire.
    #[test]
    fn test_check_flush_trigger_below_hwm_gate() {
        // 56 entries fill hashbrown's raw=64 table exactly: capacity=56 (below HWM=100)
        // and free_entries=0 (below overhead=1, so low_free_entries would fire).
        let hwm = 100;
        let lwm = 50;
        let index = new_should_write_through_for_test(Some((hwm, lwm)));
        for _ in 0..56 {
            let pubkey = solana_pubkey::new_rand();
            let entry = Box::new(AccountMapEntry::new(
                SlotList::from([(0, 0)]),
                1,
                AccountMapEntryMeta::new_dirty(&index.storage, true),
            ));
            index.map_internal.write().unwrap().insert(pubkey, entry);
        }

        let map = index.map_internal.read().unwrap();
        let len = map.len();
        let capacity = map.capacity();
        let free_entries = capacity.saturating_sub(len);
        drop(map);

        // Confirm that without the gate that low free entries would fire
        assert!(
            index
                .storage
                .should_evict_based_on_free_entries(free_entries)
        );

        // But with the gate, check_flush_trigger returns false
        assert!(!index.check_flush_trigger());
    }

    /// Once capacity has cleared the low-water mark, check_flush_trigger must still return
    /// false when the entry count is below the high-water mark and free-entry headroom exceeds
    /// the configured overhead.
    #[test]
    fn test_check_flush_trigger_below_thresholds() {
        // 60 entries push capacity to 112 (above LWM=50), len stays below HWM=100,
        // and free_entries (52) far exceeds overhead (1) — both conditions report false.
        let hwm = 100;
        let lwm = 50;
        let index = new_should_write_through_for_test(Some((hwm, lwm)));
        for _ in 0..60 {
            let pubkey = solana_pubkey::new_rand();
            let entry = Box::new(AccountMapEntry::new(
                SlotList::from([(0, 0)]),
                1,
                AccountMapEntryMeta::new_dirty(&index.storage, true),
            ));
            index.map_internal.write().unwrap().insert(pubkey, entry);
        }
        assert!(index.map_internal.read().unwrap().capacity() > lwm);

        assert!(!index.check_flush_trigger());
    }

    /// When the entry count crosses the high-water mark, check_flush_trigger returns true
    /// and the count-based trigger stat is incremented.
    #[test]
    fn test_check_flush_trigger_high_count() {
        let hwm = 2;
        let lwm = 1;
        // high_water_mark=2: inserting 4 entries puts the bin past the count threshold.
        let index = new_should_write_through_for_test(Some((hwm, lwm)));
        for _ in 0..4 {
            let pubkey = solana_pubkey::new_rand();
            let entry = Box::new(AccountMapEntry::new(
                SlotList::from([(0, 0)]),
                1,
                AccountMapEntryMeta::new_dirty(&index.storage, true),
            ));
            index.map_internal.write().unwrap().insert(pubkey, entry);
        }

        assert!(index.check_flush_trigger());
    }

    /// reallocate_to_clear_tombstones must rebuild the bin's hashmap so that all
    /// remaining entries survive and `capacity()` recovers from the drop caused by
    /// tombstones consuming `growth_left`.
    #[test]
    fn test_reallocate_to_clear_tombstones_preserves_entries() {
        // Reallocate only runs in Threshold mode. Pick high_water_mark=99 so
        // target_entries=100, matching the natural backing size for 100 inserts.
        let hwm = 99;
        let lwm = 70;
        let index = new_should_write_through_for_test(Some((hwm, lwm)));

        // Insert enough entries to settle the bin's hashmap at a stable capacity.
        let num_inserts = 100;
        let num_removes = 30;
        let pubkeys: Vec<_> = (0..num_inserts)
            .map(|_| solana_pubkey::new_rand())
            .collect();
        {
            let mut map = index.map_internal.write().unwrap();
            for pubkey in &pubkeys {
                let entry = Box::new(AccountMapEntry::new(
                    SlotList::from([(0, 42)]),
                    1,
                    AccountMapEntryMeta::new_dirty(&index.storage, true),
                ));
                map.insert(*pubkey, entry);
            }
        }
        let capacity_after_inserts = index.map_internal.read().unwrap().capacity();

        // Remove a portion of the entries to create tombstones. Hashbrown reduces capacity
        // for each tombstone created, so we should see a capacity drop here.
        let mut map = index.map_internal.write().unwrap();
        for pubkey in &pubkeys[..num_removes] {
            map.remove(pubkey);
        }
        drop(map);

        let capacity_after_removes = index.map_internal.read().unwrap().capacity();

        // Verify that capacity dropped due to added tombstones
        assert!(capacity_after_removes < capacity_after_inserts);

        index.reallocate_to_clear_tombstones();

        let map = index.map_internal.read().unwrap();

        // All remaining entries should survive the realloc.
        assert_eq!(map.len(), num_inserts - num_removes);
        for pubkey in &pubkeys[num_removes..] {
            assert!(map.contains_key(pubkey));
        }

        // Tombstones cleared: the new map sized for `len()` lands on the same raw
        // bucket count as before the removes, so capacity is back to its post-insert
        // value.
        assert_eq!(map.capacity(), capacity_after_inserts);
        drop(map);

        assert_eq!(
            index
                .stats()
                .num_hashmap_reallocates
                .load(Ordering::Relaxed),
            1
        );
    }

    /// Minimal mode evicts everything on every flush, so the per-pass HashMap
    /// rebuild would be pure waste. Verify `evict_from_cache` skips it.
    #[test]
    fn test_reallocate_skipped_in_minimal_mode() {
        let index = new_disk_buckets_for_test::<u64>();
        assert!(index.storage.threshold_entries_per_bin.is_none());

        let current_age = index.storage.current_age();
        let pubkeys: Vec<_> = (0..100).map(|_| solana_pubkey::new_rand()).collect();
        {
            let mut map = index.map_internal.write().unwrap();
            for pubkey in &pubkeys {
                let entry = AccountMapEntry::new(
                    SlotList::from([(/*slot*/ 0, /*info*/ 0)]),
                    /*ref_count*/ 1,
                    AccountMapEntryMeta::new_clean(&index.storage),
                );
                entry.set_age(current_age);
                map.insert(*pubkey, Box::new(entry));
            }
        }

        index.evict_from_cache(&pubkeys, current_age, /*ages_flushing_now*/ 0);

        // All entries were evicted...
        assert_eq!(index.map_internal.read().unwrap().len(), 0);
        // ...but reallocate must not have run in Minimal mode.
        assert_eq!(
            index
                .stats()
                .num_hashmap_reallocates
                .load(Ordering::Relaxed),
            0
        );
    }
}
