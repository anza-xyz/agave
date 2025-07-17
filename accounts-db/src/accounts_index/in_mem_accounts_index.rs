use {
    crate::{
        accounts_index::{
            account_map_entry::{
                AccountMapEntry, AccountMapEntryMeta, PreAllocatedAccountMapEntry,
            },
            DiskIndexValue, IndexValue, RefCount, SlotList, UpsertReclaim,
        },
        bucket_map_holder::{Age, AtomicAge, BucketMapHolder},
        bucket_map_holder_stats::BucketMapHolderStats,
        pubkey_bins::PubkeyBinCalculator24,
    },
    rand::{thread_rng, Rng},
    solana_bucket_map::bucket_api::BucketApi,
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    std::{
        collections::{hash_map::Entry, HashMap, HashSet},
        fmt::Debug,
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

#[derive(Debug)]
pub struct PossibleEvictions<T: IndexValue> {
    /// vec per age in the future, up to size 'ages_to_stay_in_cache'
    possible_evictions: Vec<FlushScanResult<T>>,
    /// next index to use into 'possible_evictions'
    /// if 'index' >= 'possible_evictions.len()', then there are no available entries
    index: usize,
}

impl<T: IndexValue> PossibleEvictions<T> {
    fn new(max_ages: Age) -> Self {
        Self {
            possible_evictions: (0..max_ages).map(|_| FlushScanResult::default()).collect(),
            index: max_ages as usize, // initially no data
        }
    }

    /// remove the possible evictions. This is required because we need ownership of the Arc strong counts to transfer to caller so entries can be removed from the accounts index
    fn get_possible_evictions(&mut self) -> Option<FlushScanResult<T>> {
        self.possible_evictions.get_mut(self.index).map(|result| {
            self.index += 1;
            // remove the list from 'possible_evictions'
            std::mem::take(result)
        })
    }

    /// clear existing data and prepare to add 'entries' more ages of data
    fn reset(&mut self, entries: Age) {
        self.possible_evictions.iter_mut().for_each(|entry| {
            entry.evictions_random.clear();
            entry.evictions_age_possible.clear();
        });
        let entries = entries as usize;
        assert!(
            entries <= self.possible_evictions.len(),
            "entries: {}, len: {}",
            entries,
            self.possible_evictions.len()
        );
        self.index = self.possible_evictions.len() - entries;
    }

    /// insert 'entry' at 'relative_age' in the future into 'possible_evictions'
    fn insert(
        &mut self,
        relative_age: Age,
        key: Pubkey,
        entry: Arc<AccountMapEntry<T>>,
        random: bool,
    ) {
        let index = self.index + (relative_age as usize);
        let list = &mut self.possible_evictions[index];
        if random {
            &mut list.evictions_random
        } else {
            &mut list.evictions_age_possible
        }
        .push((key, entry));
    }
}

// one instance of this represents one bin of the accounts index.
pub struct InMemAccountsIndex<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    last_age_flushed: AtomicAge,

    // backing store
    map_internal: RwLock<HashMap<Pubkey, Arc<AccountMapEntry<T>>, ahash::RandomState>>,
    storage: Arc<BucketMapHolder<T, U>>,
    _bin: usize,
    pub(crate) lowest_pubkey: Pubkey,
    pub(crate) highest_pubkey: Pubkey,

    bucket: Option<Arc<BucketApi<(Slot, U)>>>,

    // set to true while this bin is being actively flushed
    flushing_active: AtomicBool,

    /// info to streamline initial index generation
    startup_info: StartupInfo<T, U>,

    /// possible evictions for next few slots coming up
    possible_evictions: RwLock<PossibleEvictions<T>>,

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

#[derive(Default, Debug)]
/// result from scanning in-mem index during flush
struct FlushScanResult<T> {
    /// pubkeys whose age indicates they may be evicted now, pending further checks.
    evictions_age_possible: Vec<(Pubkey, Arc<AccountMapEntry<T>>)>,
    /// pubkeys chosen to evict based on random eviction
    evictions_random: Vec<(Pubkey, Arc<AccountMapEntry<T>>)>,
}

impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> InMemAccountsIndex<T, U> {
    pub fn new(storage: &Arc<BucketMapHolder<T, U>>, bin: usize) -> Self {
        let num_ages_to_distribute_flushes = Age::MAX - storage.ages_to_stay_in_cache;
        let bin_calc = PubkeyBinCalculator24::new(storage.bins);
        let lowest_pubkey = bin_calc.lowest_pubkey_from_bin(bin);
        let highest_pubkey = bin_calc.highest_pubkey_from_bin(bin);
        Self {
            map_internal: RwLock::default(),
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
            possible_evictions: RwLock::new(PossibleEvictions::new(1)),
            // Spread out the scanning across all ages within the window.
            // This causes us to scan 1/N of the bins each 'Age'
            remaining_ages_to_skip_flushing: AtomicAge::new(
                thread_rng().gen_range(0..num_ages_to_distribute_flushes),
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

    /// Release entire in-mem hashmap to free all memory associated with it.
    /// Idea is that during startup we needed a larger map than we need during runtime.
    /// When using disk-buckets, in-mem index grows over time with dynamic use and then shrinks, in theory back to 0.
    pub fn shrink_to_fit(&self) {
        // shrink_to_fit could be quite expensive on large map sizes, which 'no disk buckets' could produce, so avoid shrinking in case we end up here
        if self.storage.is_disk_index_enabled() {
            self.map_internal.write().unwrap().shrink_to_fit();
        }
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
            entry_disk
        })
    }

    /// lookup 'pubkey' in disk map.
    /// If it is found, convert it to a cache entry and return the cache entry.
    /// Cache entries from this function will always not be dirty.
    fn load_account_entry_from_disk(&self, pubkey: &Pubkey) -> Option<Arc<AccountMapEntry<T>>> {
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
        callback: impl for<'a> FnOnce(Option<&'a Arc<AccountMapEntry<T>>>) -> RT,
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
        self.get_internal(pubkey, |entry| callback(entry.map(Arc::as_ref)))
    }

    /// lookup 'pubkey' in the index (in_mem or disk).
    /// call 'callback' whether found or not
    pub(super) fn get_internal_cloned<RT>(
        &self,
        pubkey: &Pubkey,
        callback: impl for<'a> FnOnce(Option<Arc<AccountMapEntry<T>>>) -> RT,
    ) -> RT {
        // SAFETY: Since we're passing the entry Arc clone to `callback`, we must
        // also add the entry to the in-mem cache.
        self.get_internal(pubkey, |entry| (true, callback(entry.cloned())))
    }

    /// lookup 'pubkey' in index (in_mem or disk).
    /// call 'callback' whether found or not
    ///
    /// # Safety
    ///
    /// If the item is on-disk (and not in-mem), add if the item is/could be made dirty
    /// *after* `callback` finishes (e.g. the entry Arc is cloned and saved by the caller),
    /// then the disk entry *must* also be added to the in-mem cache.
    ///
    /// Prefer `get_internal_inner()` or `get_internal_cloned()` for safe alternatives.
    pub(super) fn get_internal<RT>(
        &self,
        pubkey: &Pubkey,
        // return true if item should be added to in_mem cache
        callback: impl for<'a> FnOnce(Option<&Arc<AccountMapEntry<T>>>) -> (bool, RT),
    ) -> RT {
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
                let entry = map.entry(*pubkey);
                match entry {
                    Entry::Occupied(occupied) => callback(Some(occupied.get())).1,
                    Entry::Vacant(vacant) => {
                        debug_assert!(!disk_entry.dirty());
                        let (add_to_cache, rt) = callback(Some(&disk_entry));
                        // We are holding a write lock to the in-memory map.
                        // This pubkey is not in the in-memory map.
                        // If the entry is now dirty, then it must be put in the cache or the modifications will be lost.
                        if add_to_cache || disk_entry.dirty() {
                            stats.inc_mem_count();
                            vacant.insert(disk_entry);
                        }
                        rt
                    }
                }
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
        entry: Entry<Pubkey, Arc<AccountMapEntry<T>>>,
    ) -> bool {
        match entry {
            Entry::Occupied(occupied) => {
                let result = self.remove_if_slot_list_empty_value(
                    occupied.get().slot_list.read().unwrap().is_empty(),
                );
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
        let entry = map.entry(pubkey);
        m.stop();
        let found = matches!(entry, Entry::Occupied(_));
        let result = self.remove_if_slot_list_empty_entry(entry);
        drop(map);

        self.update_entry_stats(m, found);
        result
    }

    /// call `user_fn` with a write lock of the slot list.
    /// Note that whether `user_fn` modifies the slot list or not, the entry in the in-mem index will always
    /// be marked as dirty. So, callers to this should ideally know they will be modifying the slot list.
    pub fn slot_list_mut<RT>(
        &self,
        pubkey: &Pubkey,
        user_fn: impl FnOnce(&mut SlotList<T>) -> RT,
    ) -> Option<RT> {
        self.get_internal_inner(pubkey, |entry| {
            (
                true,
                entry.map(|entry| {
                    let result = user_fn(&mut entry.slot_list.write().unwrap());
                    // note that to be safe here, we ALWAYS mark the entry as dirty
                    entry.set_dirty(true);
                    result
                }),
            )
        })
    }

    /// update 'entry' with 'new_value'
    fn update_slot_list_entry(
        &self,
        entry: &AccountMapEntry<T>,
        new_value: PreAllocatedAccountMapEntry<T>,
        other_slot: Option<Slot>,
        reclaims: &mut SlotList<T>,
        reclaim: UpsertReclaim,
    ) {
        let new_value: (Slot, T) = new_value.into();
        let mut upsert_cached = new_value.1.is_cached();
        if Self::lock_and_update_slot_list(entry, new_value, other_slot, reclaims, reclaim) > 1 {
            // if slot list > 1, then we are going to hold this entry in memory until it gets set back to 1
            upsert_cached = true;
        }
        self.set_age_to_future(entry, upsert_cached);
    }

    /// Insert a cached entry into the accounts index
    /// If the entry is already present, just mark dirty and set the age to the future
    /// Code is required just for test for now, but will be used in future PRs so not putting in the test area
    #[cfg(test)]
    fn cache_entry_at_slot(&self, entry: &AccountMapEntry<T>, slot: Slot, account_info: T) {
        let mut slot_list = entry.slot_list.write().unwrap();
        if !slot_list
            .iter()
            .any(|(existing_slot, _)| *existing_slot == slot)
        {
            slot_list.push((slot, account_info));
        }
        entry.set_dirty(true);
        self.set_age_to_future(entry, true);
    }

    pub fn upsert(
        &self,
        pubkey: &Pubkey,
        new_value: PreAllocatedAccountMapEntry<T>,
        other_slot: Option<Slot>,
        reclaims: &mut SlotList<T>,
        reclaim: UpsertReclaim,
    ) {
        self.get_or_create_index_entry_for_pubkey(pubkey, |entry| {
            self.update_slot_list_entry(entry, new_value, other_slot, reclaims, reclaim)
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
                let mut m = Measure::start("entry");
                let mut map = self.map_internal.write().unwrap();
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
                            Arc::new(AccountMapEntry::new(
                                vec![],
                                0,
                                AccountMapEntryMeta::new_dirty(&self.storage, true),
                            ))
                        };
                        callback(&new_value);

                        // Ensure that after callback there is an item in the slot list
                        assert_ne!(
                            new_value.slot_list.read().unwrap().len(),
                            0,
                            "Callback must insert item into slot list"
                        );
                        assert!(new_value.dirty());
                        vacant.insert(new_value);
                        self.stats().inc_mem_count();
                    }
                };

                drop(map);
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
    /// if 'other_slot' is some, then also remove any entries in the slot list that are at 'other_slot'
    /// return resulting len of slot list
    pub fn lock_and_update_slot_list(
        current: &AccountMapEntry<T>,
        new_value: (Slot, T),
        other_slot: Option<Slot>,
        reclaims: &mut SlotList<T>,
        reclaim: UpsertReclaim,
    ) -> usize {
        let mut slot_list = current.slot_list.write().unwrap();
        let (slot, new_entry) = new_value;
        let addref = Self::update_slot_list(
            &mut slot_list,
            slot,
            new_entry,
            other_slot,
            reclaims,
            reclaim,
        );
        if addref {
            current.addref();
        }
        current.set_dirty(true);
        slot_list.len()
    }

    /// modifies slot_list
    /// any entry at 'slot' or slot 'other_slot' is replaced with 'account_info'.
    /// or, 'account_info' is appended to the slot list if the slot did not exist previously.
    /// returns true if caller should addref
    /// conditions when caller should addref:
    ///   'account_info' does NOT represent a cached storage (the slot is being flushed from the cache)
    /// AND
    ///   previous slot_list entry AT 'slot' did not exist (this is the first time this account was modified in this "slot"), or was previously cached (the storage is now being flushed from the cache)
    /// Note that even if entry DID exist at 'other_slot', the above conditions apply.
    fn update_slot_list(
        slot_list: &mut SlotList<T>,
        slot: Slot,
        account_info: T,
        mut other_slot: Option<Slot>,
        reclaims: &mut SlotList<T>,
        reclaim: UpsertReclaim,
    ) -> bool {
        let mut addref = !account_info.is_cached();

        if other_slot == Some(slot) {
            other_slot = None; // redundant info, so ignore
        }

        // There may be 0..=2 dirty accounts found (one at 'slot' and one at 'other_slot')
        // that are already in the slot list.  Since the first one found will be swapped with the
        // new account, if a second one is found, we cannot swap again. Instead, just remove it.
        let mut found_slot = false;
        let mut found_other_slot = false;
        (0..slot_list.len())
            .rev() // rev since we delete from the list in some cases
            .for_each(|slot_list_index| {
                let (cur_slot, cur_account_info) = &slot_list[slot_list_index];
                let matched_slot = *cur_slot == slot;
                if matched_slot || Some(*cur_slot) == other_slot {
                    // make sure neither 'slot' nor 'other_slot' are in the slot list more than once
                    let matched_other_slot = !matched_slot;
                    assert!(
                        !(found_slot && matched_slot || matched_other_slot && found_other_slot),
                        "{slot_list:?}, slot: {slot}, other_slot: {other_slot:?}"
                    );

                    let is_cur_account_cached = cur_account_info.is_cached();

                    let reclaim_item = if !(found_slot || found_other_slot) {
                        // first time we found an entry in 'slot' or 'other_slot', so replace it in-place.
                        // this may be the only instance we find
                        std::mem::replace(&mut slot_list[slot_list_index], (slot, account_info))
                    } else {
                        // already replaced one entry, so this one has to be removed
                        slot_list.remove(slot_list_index)
                    };
                    match reclaim {
                        UpsertReclaim::PopulateReclaims => {
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

                    if matched_slot {
                        found_slot = true;
                    } else {
                        found_other_slot = true;
                    }
                    if !is_cur_account_cached {
                        // current info at 'slot' is NOT cached, so we should NOT addref. This slot already has a ref count for this pubkey.
                        addref = false;
                    }
                }
            });
        if !found_slot && !found_other_slot {
            // if we make it here, we did not find the slot in the list
            slot_list.push((slot, account_info));
        }
        addref
    }

    // convert from raw data on disk to AccountMapEntry, set to age in future
    fn disk_to_cache_entry(
        &self,
        slot_list: SlotList<U>,
        ref_count: RefCount,
    ) -> Arc<AccountMapEntry<T>> {
        Arc::new(AccountMapEntry::new(
            slot_list
                .into_iter()
                .map(|(slot, info)| (slot, info.into()))
                .collect(),
            ref_count,
            AccountMapEntryMeta::new_clean(&self.storage),
        ))
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

                let slot_list = occupied.get().slot_list.read().unwrap();

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

                let updated_slot_list_len = InMemAccountsIndex::<T, U>::lock_and_update_slot_list(
                    occupied.get(),
                    (slot, account_info),
                    None, // should be None because we don't expect a different slot # during index generation
                    &mut Vec::default(),
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
                        &mut Vec::default(),
                        UpsertReclaim::IgnoreReclaims,
                    );
                    vacant.insert(disk_entry);
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

    /// returns true if a dice roll indicates this call should result in a random eviction.
    /// This causes non-determinism in cache contents per validator.
    fn random_chance_of_eviction() -> bool {
        // random eviction
        const N: usize = 1000;
        // 1/N chance of eviction
        thread_rng().gen_range(0..N) == 0
    }

    /// assumes 1 entry in the slot list. Ignores overhead of the HashMap and such
    pub const fn approx_size_of_one_entry() -> usize {
        size_of::<(Slot, T)>() + size_of::<Pubkey>() + size_of::<AccountMapEntry<T>>()
    }

    fn should_evict_based_on_age(
        current_age: Age,
        entry: &AccountMapEntry<T>,
        startup: bool,
        ages_flushing_now: Age,
    ) -> bool {
        startup || current_age.wrapping_sub(entry.age()) <= ages_flushing_now
    }

    /// return true if 'entry' should be evicted from the in-mem index
    fn should_evict_from_mem<'a>(
        &self,
        current_age: Age,
        entry: &'a Arc<AccountMapEntry<T>>,
        startup: bool,
        update_stats: bool,
        ages_flushing_now: Age,
    ) -> (bool, Option<std::sync::RwLockReadGuard<'a, SlotList<T>>>) {
        // this could be tunable dynamically based on memory pressure
        // we could look at more ages or we could throw out more items we are choosing to keep in the cache
        if Self::should_evict_based_on_age(current_age, entry, startup, ages_flushing_now) {
            if entry.ref_count() != 1 {
                Self::update_stat(&self.stats().held_in_mem.ref_count, 1);
                (false, None)
            } else {
                // only read the slot list if we are planning to throw the item out
                let slot_list = entry.slot_list.read().unwrap();
                if slot_list.len() != 1 {
                    if update_stats {
                        Self::update_stat(&self.stats().held_in_mem.slot_list_len, 1);
                    }
                    (false, None) // keep 0 and > 1 slot lists in mem. They will be cleaned or shrunk soon.
                } else {
                    // keep items with slot lists that contained cached items
                    let evict = !slot_list.iter().any(|(_, info)| info.is_cached());
                    if !evict && update_stats {
                        Self::update_stat(&self.stats().held_in_mem.slot_list_cached, 1);
                    }
                    (evict, if evict { Some(slot_list) } else { None })
                }
            }
        } else {
            (false, None)
        }
    }

    /// fill in `possible_evictions` from `iter` by checking age
    fn gather_possible_evictions<'a>(
        iter: impl Iterator<Item = (&'a Pubkey, &'a Arc<AccountMapEntry<T>>)>,
        possible_evictions: &mut PossibleEvictions<T>,
        startup: bool,
        current_age: Age,
        ages_flushing_now: Age,
        can_randomly_flush: bool,
    ) {
        for (k, v) in iter {
            let mut random = false;
            if !startup && current_age.wrapping_sub(v.age()) > ages_flushing_now {
                if !can_randomly_flush || !Self::random_chance_of_eviction() {
                    // not planning to evict this item from memory within 'ages_flushing_now' ages
                    continue;
                }
                random = true;
            }

            possible_evictions.insert(0, *k, Arc::clone(v), random);
        }
    }

    /// scan loop
    /// holds read lock
    /// identifies items which are potential candidates to evict
    fn flush_scan(
        &self,
        current_age: Age,
        startup: bool,
        _flush_guard: &FlushGuard,
        ages_flushing_now: Age,
    ) -> FlushScanResult<T> {
        let mut possible_evictions = self.possible_evictions.write().unwrap();
        possible_evictions.reset(1);
        let m;
        {
            let map = self.map_internal.read().unwrap();
            m = Measure::start("flush_scan"); // we don't care about lock time in this metric - bg threads can wait
            Self::gather_possible_evictions(
                map.iter(),
                &mut possible_evictions,
                startup,
                current_age,
                ages_flushing_now,
                true,
            );
        }
        Self::update_time_stat(&self.stats().flush_scan_us, m);

        possible_evictions.get_possible_evictions().unwrap()
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

        let duplicates = duplicates_put_on_disk
            .into_iter()
            .chain(duplicates.into_iter().map(|(slot, key, info)| {
                let entry = PreAllocatedAccountMapEntry::new(slot, info, &self.storage, true);
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
        if !iterate_for_age && !startup {
            // no need to age, so no need to flush this bucket
            // but, at startup we want to evict from buckets as fast as possible if any items exist
            return;
        }

        if startup {
            self.write_startup_info_to_disk();
        }

        let ages_flushing_now = if iterate_for_age && !startup {
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
        } else {
            // just 1 age to flush. 0 means age == age
            0
        };

        Self::update_stat(&self.stats().buckets_scanned, 1);

        // scan in-mem map for items that we may evict
        let FlushScanResult {
            mut evictions_age_possible,
            mut evictions_random,
        } = self.flush_scan(current_age, startup, flush_guard, ages_flushing_now);

        // write to disk outside in-mem map read lock
        {
            let mut evictions_age = Vec::with_capacity(evictions_age_possible.len());
            if !evictions_age_possible.is_empty() || !evictions_random.is_empty() {
                let disk = self.bucket.as_ref().unwrap();
                let mut flush_entries_updated_on_disk = 0;
                let mut flush_should_evict_us = 0;
                // we don't care about lock time in this metric - bg threads can wait
                let m = Measure::start("flush_update");

                // consider whether to write to disk for all the items we may evict, whether evicting due to age or random
                for (is_random, check_for_eviction_and_dirty) in [
                    (false, &mut evictions_age_possible),
                    (true, &mut evictions_random),
                ] {
                    for (k, v) in check_for_eviction_and_dirty.drain(..) {
                        let mut slot_list = None;
                        if !is_random {
                            let mut mse = Measure::start("flush_should_evict");
                            let (evict_for_age, slot_list_temp) = self.should_evict_from_mem(
                                current_age,
                                &v,
                                startup,
                                true,
                                ages_flushing_now,
                            );
                            slot_list = slot_list_temp;
                            mse.stop();
                            flush_should_evict_us += mse.as_us();
                            if evict_for_age {
                                evictions_age.push(k);
                            } else {
                                // not evicting, so don't write, even if dirty
                                continue;
                            }
                        } else if v.ref_count() != 1 {
                            continue;
                        }
                        if is_random && v.dirty() {
                            // Don't randomly evict dirty entries. Evicting dirty entries results in us writing entries with many slot list elements for example, unnecessarily.
                            // So, only randomly evict entries that lru would say don't throw away and were just read (or were dirty and written, but could not be evicted).
                            continue;
                        }

                        // if we are evicting it, then we need to update disk if we're dirty
                        if v.clear_dirty() {
                            // step 1: clear the dirty flag
                            // step 2: perform the update on disk based on the fields in the entry
                            // If a parallel operation dirties the item again - even while this flush is occurring,
                            //  the last thing the writer will do, after updating contents, is set_dirty(true)
                            //  That prevents dropping an item from cache before disk is updated to latest in mem.
                            // It is possible that the item in the cache is marked as dirty while these updates are happening. That is ok.
                            //  The dirty will be picked up and the item will be prevented from being evicted.

                            // may have to loop if disk has to grow and we have to retry the write
                            loop {
                                let disk_resize = {
                                    let slot_list = slot_list
                                        .take()
                                        .unwrap_or_else(|| v.slot_list.read().unwrap());
                                    // Check the ref count and slot list one more time before flushing.
                                    // It is possible the foreground has updated this entry since
                                    // we last checked above in `should_evict_from_mem()`.
                                    // If the entry *was* updated, re-mark it as dirty then
                                    // skip to the next pubkey/entry.
                                    let ref_count = v.ref_count();
                                    if ref_count != 1 || slot_list.len() != 1 {
                                        v.set_dirty(true);
                                        break;
                                    }
                                    disk.try_write(
                                        &k,
                                        (
                                            &slot_list
                                                .iter()
                                                .map(|(slot, info)| (*slot, (*info).into()))
                                                .collect::<Vec<_>>(),
                                            ref_count,
                                        ),
                                    )
                                };
                                match disk_resize {
                                    Ok(_) => {
                                        // successfully written to disk
                                        flush_entries_updated_on_disk += 1;
                                        break;
                                    }
                                    Err(err) => {
                                        // disk needs to resize. This item did not get written. Resize and try again.
                                        let m = Measure::start("flush_grow");
                                        disk.grow(err);
                                        Self::update_time_stat(&self.stats().flush_grow_us, m);
                                    }
                                }
                            }
                        }
                    }
                }
                Self::update_time_stat(&self.stats().flush_update_us, m);
                Self::update_stat(&self.stats().flush_should_evict_us, flush_should_evict_us);
                Self::update_stat(
                    &self.stats().flush_entries_updated_on_disk,
                    flush_entries_updated_on_disk,
                );
                // remove the 'v'
                let evictions_random = evictions_random
                    .into_iter()
                    .map(|(k, _v)| k)
                    .collect::<Vec<_>>();

                let m = Measure::start("flush_evict");
                self.evict_from_cache(
                    evictions_age,
                    current_age,
                    startup,
                    false,
                    ages_flushing_now,
                );
                self.evict_from_cache(
                    evictions_random,
                    current_age,
                    startup,
                    true,
                    ages_flushing_now,
                );
                Self::update_time_stat(&self.stats().flush_evict_us, m);
            }

            if iterate_for_age {
                // completed iteration of the buckets at the current age
                assert_eq!(current_age, self.storage.current_age());
                self.set_has_aged(current_age, can_advance_age);
            }
        }
    }

    // evict keys in 'evictions' from in-mem cache, likely due to age
    fn evict_from_cache(
        &self,
        evictions: Vec<Pubkey>,
        current_age: Age,
        startup: bool,
        randomly_evicted: bool,
        ages_flushing_now: Age,
    ) {
        if evictions.is_empty() {
            return;
        }

        let next_age_on_failure = self.storage.future_age_to_flush(false);
        let mut failed = 0;
        let mut evicted = 0;
        // chunk these so we don't hold the write lock too long
        for evictions in evictions.chunks(50) {
            let mut map = self.map_internal.write().unwrap();
            for k in evictions {
                if let Entry::Occupied(occupied) = map.entry(*k) {
                    let v = occupied.get();
                    if Arc::strong_count(v) > 1 {
                        // someone is holding the value arc's ref count and could modify it, so do not evict
                        failed += 1;
                        v.try_exchange_age(next_age_on_failure, current_age);
                        continue;
                    }

                    if v.dirty()
                        || (!randomly_evicted
                            && !Self::should_evict_based_on_age(
                                current_age,
                                v,
                                startup,
                                ages_flushing_now,
                            ))
                    {
                        // marked dirty or bumped in age after we looked above
                        // these evictions will be handled in later passes (at later ages)
                        // but, at startup, everything is ready to age out if it isn't dirty
                        failed += 1;
                        continue;
                    }

                    // all conditions for eviction succeeded, so really evict item from in-mem cache
                    evicted += 1;
                    occupied.remove();
                }
            }
            if map.is_empty() {
                map.shrink_to_fit();
            }
        }
        self.stats().sub_mem_count(evicted);
        Self::update_stat(&self.stats().flush_entries_evicted_from_mem, evicted as u64);
        Self::update_stat(&self.stats().failed_to_evict, failed as u64);
    }

    pub fn stats(&self) -> &BucketMapHolderStats {
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::accounts_index::{AccountsIndexConfig, BINS_FOR_TESTING},
        assert_matches::assert_matches,
        itertools::Itertools,
    };

    fn new_for_test<T: IndexValue>() -> InMemAccountsIndex<T, T> {
        let holder = Arc::new(BucketMapHolder::new(
            BINS_FOR_TESTING,
            &AccountsIndexConfig::default(),
            1,
        ));
        let bin = 0;
        InMemAccountsIndex::new(&holder, bin)
    }

    fn new_disk_buckets_for_test<T: IndexValue>() -> InMemAccountsIndex<T, T> {
        let holder = Arc::new(BucketMapHolder::new(
            BINS_FOR_TESTING,
            &AccountsIndexConfig::default(),
            1,
        ));
        let bin = 0;
        let bucket = InMemAccountsIndex::new(&holder, bin);
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
            assert!(entry.slot_list.read().unwrap().is_empty());
            assert_eq!(entry.ref_count(), 0);
            assert!(entry.dirty());
            accounts_index.cache_entry_at_slot(entry, slot, 0);
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
        let entry = Arc::new(AccountMapEntry::new(
            vec![(0, 42)],
            1,
            AccountMapEntryMeta::new_dirty(&accounts_index.storage, true),
        ));
        accounts_index
            .map_internal
            .write()
            .unwrap()
            .insert(pubkey, Arc::clone(&entry));

        let mut callback_called = false;
        accounts_index.get_or_create_index_entry_for_pubkey(&pubkey, |entry| {
            assert_eq!(entry.slot_list.read().unwrap().len(), 1);
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

        // Ensure the entry is not found in meory
        let mut found = false;
        accounts_index.get_only_in_mem(&pubkey, false, |entry| {
            found = entry.is_some();
        });
        assert!(!found);

        let mut callback_called = false;
        accounts_index.get_or_create_index_entry_for_pubkey(&pubkey, |entry| {
            assert_eq!(entry.slot_list.read().unwrap().len(), 1);
            assert_eq!(entry.ref_count(), 1);
            assert!(!entry.dirty()); // Entry loaded from disk should not be dirty
            accounts_index.cache_entry_at_slot(entry, slot, 0);
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
    fn test_should_evict_from_mem_ref_count() {
        for ref_count in [0, 1, 2] {
            let bucket = new_for_test::<u64>();
            let startup = false;
            let current_age = 0;
            let one_element_slot_list = vec![(0, 0)];
            let one_element_slot_list_entry = Arc::new(AccountMapEntry::new(
                one_element_slot_list,
                ref_count,
                AccountMapEntryMeta::default(),
            ));

            // exceeded budget
            assert_eq!(
                bucket
                    .should_evict_from_mem(
                        current_age,
                        &one_element_slot_list_entry,
                        startup,
                        false,
                        1,
                    )
                    .0,
                ref_count == 1
            );
        }
    }

    #[test]
    fn test_gather_possible_evictions() {
        solana_logger::setup();
        let startup = false;
        let ref_count = 1;
        let pks = (0..=255)
            .map(|i| Pubkey::from([i as u8; 32]))
            .collect::<Vec<_>>();
        let accounts = (0..=255)
            .map(|age| {
                let one_element_slot_list = vec![(0, 0)];
                let one_element_slot_list_entry = Arc::new(AccountMapEntry::new(
                    one_element_slot_list,
                    ref_count,
                    AccountMapEntryMeta::default(),
                ));
                one_element_slot_list_entry.set_age(age);
                one_element_slot_list_entry
            })
            .collect::<Vec<_>>();
        let both = pks.iter().zip(accounts.iter()).collect::<Vec<_>>();

        for current_age in 0..=255 {
            for ages_flushing_now in 0..=255 {
                let mut possible_evictions = PossibleEvictions::new(1);
                possible_evictions.reset(1);
                InMemAccountsIndex::<u64, u64>::gather_possible_evictions(
                    both.iter().cloned(),
                    &mut possible_evictions,
                    startup,
                    current_age,
                    ages_flushing_now,
                    false, // true=can_randomly_flush
                );
                let evictions = possible_evictions.possible_evictions.pop().unwrap();
                assert_eq!(
                    evictions.evictions_age_possible.len(),
                    1 + ages_flushing_now as usize
                );
                evictions.evictions_age_possible.iter().for_each(|(_k, v)| {
                    assert!(
                        InMemAccountsIndex::<u64, u64>::should_evict_based_on_age(
                            current_age,
                            v,
                            startup,
                            ages_flushing_now,
                        ),
                        "current_age: {}, age: {}, ages_flushing_now: {}",
                        current_age,
                        v.age(),
                        ages_flushing_now
                    );
                });
            }
        }
    }

    #[test]
    fn test_should_evict_from_mem() {
        solana_logger::setup();
        let bucket = new_for_test::<u64>();
        let mut startup = false;
        let mut current_age = 0;
        let ref_count = 1;
        let one_element_slot_list = vec![(0, 0)];
        let one_element_slot_list_entry = Arc::new(AccountMapEntry::new(
            one_element_slot_list,
            ref_count,
            AccountMapEntryMeta::default(),
        ));

        // empty slot list
        assert!(
            !bucket
                .should_evict_from_mem(
                    current_age,
                    &Arc::new(AccountMapEntry::new(
                        vec![],
                        ref_count,
                        AccountMapEntryMeta::default()
                    )),
                    startup,
                    false,
                    0,
                )
                .0
        );
        // 1 element slot list
        assert!(
            bucket
                .should_evict_from_mem(
                    current_age,
                    &one_element_slot_list_entry,
                    startup,
                    false,
                    0,
                )
                .0
        );
        // 2 element slot list
        assert!(
            !bucket
                .should_evict_from_mem(
                    current_age,
                    &Arc::new(AccountMapEntry::new(
                        vec![(0, 0), (1, 1)],
                        ref_count,
                        AccountMapEntryMeta::default()
                    )),
                    startup,
                    false,
                    0,
                )
                .0
        );

        {
            let bucket = new_for_test::<f64>();
            // 1 element slot list with a CACHED item - f64 acts like cached
            assert!(
                !bucket
                    .should_evict_from_mem(
                        current_age,
                        &Arc::new(AccountMapEntry::new(
                            vec![(0, 0.0)],
                            ref_count,
                            AccountMapEntryMeta::default()
                        )),
                        startup,
                        false,
                        0,
                    )
                    .0
            );
        }

        // 1 element slot list, age is now
        assert!(
            bucket
                .should_evict_from_mem(
                    current_age,
                    &one_element_slot_list_entry,
                    startup,
                    false,
                    0,
                )
                .0
        );

        // 1 element slot list, but not current age
        current_age = 1;
        assert!(
            !bucket
                .should_evict_from_mem(
                    current_age,
                    &one_element_slot_list_entry,
                    startup,
                    false,
                    0,
                )
                .0
        );

        // 1 element slot list, but at startup and age not current
        startup = true;
        assert!(
            bucket
                .should_evict_from_mem(
                    current_age,
                    &one_element_slot_list_entry,
                    startup,
                    false,
                    0,
                )
                .0
        );
    }

    #[test]
    fn test_age() {
        solana_logger::setup();
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
    fn test_update_slot_list_other() {
        solana_logger::setup();
        let reclaim = UpsertReclaim::PopulateReclaims;
        let new_slot = 0;
        let info = 1;
        let other_value = info + 1;
        let at_new_slot = (new_slot, info);
        let unique_other_slot = new_slot + 1;
        for other_slot in [Some(new_slot), Some(unique_other_slot), None] {
            let mut reclaims = Vec::default();
            let mut slot_list = Vec::default();
            // upserting into empty slot_list, so always addref
            assert!(
                InMemAccountsIndex::<u64, u64>::update_slot_list(
                    &mut slot_list,
                    new_slot,
                    info,
                    other_slot,
                    &mut reclaims,
                    reclaim
                ),
                "other_slot: {other_slot:?}"
            );
            assert_eq!(slot_list, vec![at_new_slot]);
            assert!(reclaims.is_empty());
        }

        // replace other
        let mut slot_list = vec![(unique_other_slot, other_value)];
        let expected_reclaims = slot_list.clone();
        let other_slot = Some(unique_other_slot);
        let mut reclaims = Vec::default();
        assert!(
            // upserting into slot_list that does NOT contain an entry at 'new_slot'
            // but, it DOES contain an entry at other_slot, so we do NOT add-ref. The assumption is that 'other_slot' is going away
            // and that the previously held add-ref is now used by 'new_slot'
            !InMemAccountsIndex::<u64, u64>::update_slot_list(
                &mut slot_list,
                new_slot,
                info,
                other_slot,
                &mut reclaims,
                reclaim
            ),
            "other_slot: {other_slot:?}"
        );
        assert_eq!(slot_list, vec![at_new_slot]);
        assert_eq!(reclaims, expected_reclaims);

        // replace other and new_slot
        let mut slot_list = vec![(unique_other_slot, other_value), (new_slot, other_value)];
        let expected_reclaims = slot_list.clone();
        let other_slot = Some(unique_other_slot);
        // upserting into slot_list that already contain an entry at 'new-slot', so do NOT addref
        let mut reclaims = Vec::default();
        assert!(
            !InMemAccountsIndex::<u64, u64>::update_slot_list(
                &mut slot_list,
                new_slot,
                info,
                other_slot,
                &mut reclaims,
                reclaim
            ),
            "other_slot: {other_slot:?}"
        );
        assert_eq!(slot_list, vec![at_new_slot]);
        assert_eq!(
            reclaims,
            expected_reclaims.into_iter().rev().collect::<Vec<_>>()
        );

        // nothing will exist at this slot
        let missing_other_slot = unique_other_slot + 1;
        let ignored_slot = 10; // bigger than is used elsewhere in the test
        let ignored_value = info + 10;

        let mut possible_initial_slot_list_contents;
        // build a list of possible contents in the slot_list prior to calling 'update_slot_list'
        {
            // up to 3 ignored slot account_info (ignored means not 'new_slot', not 'other_slot', but different slot #s which could exist in the slot_list initially)
            possible_initial_slot_list_contents = (0..3)
                .map(|i| (ignored_slot + i, ignored_value + i))
                .collect::<Vec<_>>();
            // account_info that already exists in the slot_list AT 'new_slot'
            possible_initial_slot_list_contents.push(at_new_slot);
            // account_info that already exists in the slot_list AT 'other_slot'
            possible_initial_slot_list_contents.push((unique_other_slot, other_value));
        }

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
                    attempts += 1;
                    // initialize slot_list prior to call to 'InMemAccountsIndex::update_slot_list'
                    // by inserting each possible entry at each possible position
                    let mut slot_list = content_source_indexes
                        .iter()
                        .map(|i| possible_initial_slot_list_contents[*i])
                        .collect::<Vec<_>>();
                    let mut expected = slot_list.clone();
                    let original = slot_list.clone();
                    let mut reclaims = Vec::default();

                    let result = InMemAccountsIndex::<u64, u64>::update_slot_list(
                        &mut slot_list,
                        new_slot,
                        info,
                        other_slot,
                        &mut reclaims,
                        reclaim,
                    );

                    // calculate expected results
                    let mut expected_reclaims = Vec::default();
                    // addref iff the slot_list did NOT previously contain an entry at 'new_slot' and it also did not contain an entry at 'other_slot'
                    let expected_result = !expected
                        .iter()
                        .any(|(slot, _info)| slot == &new_slot || Some(*slot) == other_slot);
                    {
                        // this is the logical equivalent of 'InMemAccountsIndex::update_slot_list', but slower (and ignoring addref)
                        expected.retain(|(slot, info)| {
                            let retain = slot != &new_slot && Some(*slot) != other_slot;
                            if !retain {
                                expected_reclaims.push((*slot, *info));
                            }
                            retain
                        });
                        expected.push((new_slot, info));
                    }
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
        assert_eq!(attempts, 1304); // complicated permutations, so make sure we ran the right #
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
            let val = Arc::new(AccountMapEntry::<u64>::default());
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
            let val = Arc::new(AccountMapEntry::<u64>::default());
            val.slot_list.write().unwrap().push((1, 1));
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
        let test = AccountMapEntry::<u64>::default();
        let info = 65;
        let mut reclaims = Vec::default();
        // first upsert, should increase
        let len = InMemAccountsIndex::<u64, u64>::lock_and_update_slot_list(
            &test,
            (1, info),
            None,
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
        assert_eq!(test.slot_list.read().unwrap().len(), len);
        assert_eq!(len, 1);
        // update to different slot, should increase
        let len = InMemAccountsIndex::<u64, u64>::lock_and_update_slot_list(
            &test,
            (2, info),
            None,
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
        assert_eq!(test.slot_list.read().unwrap().len(), len);
        assert_eq!(len, 2);
        // update to same slot, should not increase
        let len = InMemAccountsIndex::<u64, u64>::lock_and_update_slot_list(
            &test,
            (2, info),
            None,
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );
        assert_eq!(test.slot_list.read().unwrap().len(), len);
        assert_eq!(len, 2);
    }
}
