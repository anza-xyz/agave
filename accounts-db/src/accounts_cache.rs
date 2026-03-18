use {
    crate::ancestors::Ancestors,
    dashmap::{DashMap, mapref::entry::Entry},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        collections::BTreeSet,
        ops::Deref,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    },
};

/// Tracks the maximum flushed root slot.
///
/// Internally stores `slot + 1` so that the default value of `0` naturally
/// represents "no flushed roots".
#[derive(Debug, Default)]
struct MaxFlushedRoot(AtomicU64);

impl MaxFlushedRoot {
    /// Returns the current max flushed root, or `None` if no root has been flushed.
    fn get(&self) -> Option<Slot> {
        self.0.load(Ordering::Acquire).checked_sub(1)
    }

    /// Atomically update to the maximum of the current value and `slot`.
    /// Stores `slot + 1` internally so 0 can represent no slots flushed
    /// Note: Will overflow at u64::MAX, but realistically should never get that high
    fn fetch_max(&self, slot: Slot) {
        self.0.fetch_max(slot + 1, Ordering::Release);
    }
}

/// Indicates whether a slot is an ancestor, an unflushed root, or a root being flushed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// Slot is in the ancestors list and not a root being flushed. Slot is guaranteed to be
    /// the newest version of the account on this fork so the cached copy can be used without
    /// checking storage
    Ancestor,
    /// Slot is in the ancestors list but is also a root being flushed. The version found in the
    /// accounts cache may not be the newest version of the account, and the storage should be
    /// checked for newer versions
    AncestorBeingFlushed,
    /// Slot is a root that has not yet been claimed for flushing. Slot is guaranteed to be the
    /// newest version of the account so the cached copy can be used without checking storage
    UnflushedRoot,
    /// Slot is a root that is currently being flushed. The version found in the accounts cache
    /// may not be the newest version of the account, and the storage should be checked for newer
    /// versions
    RootBeingFlushed,
}

#[derive(Debug)]
pub struct SlotCache {
    cache: DashMap<Pubkey, Arc<CachedAccount>, PubkeyHasherBuilder>,
    same_account_writes: AtomicU64,
    same_account_writes_size: AtomicU64,
    unique_account_writes_size: AtomicU64,
    /// The size of account data stored in `cache` (just this slot), in bytes
    size: AtomicU64,
    /// The size of account data stored in the whole AccountsCache, in bytes
    total_size: Arc<AtomicU64>,
    is_frozen: AtomicBool,
    /// The number of accounts stored in `cache` (just this slot)
    accounts_count: AtomicU64,
    /// The number of accounts stored in the whole AccountsCache
    total_accounts_count: Arc<AtomicU64>,
}

impl Drop for SlotCache {
    fn drop(&mut self) {
        // broader cache no longer holds our size/counts in memory
        self.total_size
            .fetch_sub(*self.size.get_mut(), Ordering::Relaxed);
        self.total_accounts_count
            .fetch_sub(*self.accounts_count.get_mut(), Ordering::Relaxed);
    }
}

impl SlotCache {
    pub fn report_slot_store_metrics(&self) {
        datapoint_info!(
            "slot_repeated_writes",
            (
                "same_account_writes",
                self.same_account_writes.load(Ordering::Relaxed),
                i64
            ),
            (
                "same_account_writes_size",
                self.same_account_writes_size.load(Ordering::Relaxed),
                i64
            ),
            (
                "unique_account_writes_size",
                self.unique_account_writes_size.load(Ordering::Relaxed),
                i64
            ),
            ("size", self.size.load(Ordering::Relaxed), i64),
            (
                "accounts_count",
                self.accounts_count.load(Ordering::Relaxed),
                i64
            )
        );
    }

    /// Insert an account into this slot's cache
    ///
    /// Returns the cached account and whether this was a new unique key for this slot
    fn insert(&self, pubkey: &Pubkey, account: AccountSharedData) -> (Arc<CachedAccount>, bool) {
        let data_len = account.data().len() as u64;
        let item = Arc::new(CachedAccount {
            account,
            pubkey: *pubkey,
        });
        let is_new_key = if let Some(old) = self.cache.insert(*pubkey, item.clone()) {
            self.same_account_writes.fetch_add(1, Ordering::Relaxed);
            self.same_account_writes_size
                .fetch_add(data_len, Ordering::Relaxed);

            let old_len = old.account.data().len() as u64;
            let grow = data_len.saturating_sub(old_len);
            if grow > 0 {
                self.size.fetch_add(grow, Ordering::Relaxed);
                self.total_size.fetch_add(grow, Ordering::Relaxed);
            } else {
                let shrink = old_len.saturating_sub(data_len);
                if shrink > 0 {
                    self.size.fetch_sub(shrink, Ordering::Relaxed);
                    self.total_size.fetch_sub(shrink, Ordering::Relaxed);
                }
            }
            false
        } else {
            self.size.fetch_add(data_len, Ordering::Relaxed);
            self.total_size.fetch_add(data_len, Ordering::Relaxed);
            self.unique_account_writes_size
                .fetch_add(data_len, Ordering::Relaxed);
            self.accounts_count.fetch_add(1, Ordering::Relaxed);
            self.total_accounts_count.fetch_add(1, Ordering::Relaxed);
            true
        };
        (item, is_new_key)
    }

    fn get_cloned(&self, pubkey: &Pubkey) -> Option<Arc<CachedAccount>> {
        self.cache
            .get(pubkey)
            // 1) Maybe can eventually use a Cow to avoid a clone on every read
            // 2) Popping is only safe if it's guaranteed that only
            //    replay/banking threads are reading from the AccountsDb
            .map(|account_ref| account_ref.value().clone())
    }

    pub fn mark_slot_frozen(&self) {
        self.is_frozen.store(true, Ordering::Release);
    }

    pub fn is_frozen(&self) -> bool {
        self.is_frozen.load(Ordering::Acquire)
    }

    pub fn total_bytes(&self) -> u64 {
        self.unique_account_writes_size.load(Ordering::Relaxed)
            + self.same_account_writes_size.load(Ordering::Relaxed)
    }
}

impl Deref for SlotCache {
    type Target = DashMap<Pubkey, Arc<CachedAccount>, PubkeyHasherBuilder>;
    fn deref(&self) -> &Self::Target {
        &self.cache
    }
}

#[derive(Debug)]
pub struct CachedAccount {
    pub account: AccountSharedData,
    pubkey: Pubkey,
}

impl CachedAccount {
    pub fn pubkey(&self) -> &Pubkey {
        &self.pubkey
    }
}

/// Maps each pubkey to (max_slot, ref_count) where max_slot is the highest slot at which the
/// pubkey has been written into the cache, and ref_count is the number of SlotCache entries that
/// currently hold the pubkey. max_slot may be stale after a removal; callers must handle a
/// look-up miss on max_slot by falling back to scanning all slots in the cache (see load_latest)
#[derive(Debug, Default)]
struct AccountsCacheIndex {
    entries: DashMap<Pubkey, (Slot, u32), PubkeyHasherBuilder>,
    // The number of unique pubkeys in the index, for reporting purposes. This is to avoid having to
    // lock each shard of the entries dashmap to count unique keys on demand
    num_unique_pubkeys: AtomicU64,
}

impl AccountsCacheIndex {
    /// Inserts an entry into the index. If the entry is already present, increase the ref count
    fn insert(&self, pubkey: &Pubkey, slot: Slot) {
        self.entries
            .entry(*pubkey)
            .and_modify(|(stored_slot, ref_count)| {
                *stored_slot = slot.max(*stored_slot);
                *ref_count += 1;
            })
            .or_insert_with(|| {
                self.num_unique_pubkeys.fetch_add(1, Ordering::Relaxed);
                (slot, 1)
            });
    }

    /// Decrement the reference count for each pubkey in `pubkeys`. Removes an entry entirely if
    /// the count reaches zero. `max_slot` is not updated; it will become stale if the removed slot
    /// is the highest slot
    fn remove(&self, pubkeys: impl IntoIterator<Item = Pubkey>) {
        for pubkey in pubkeys {
            let Entry::Occupied(mut occupied_entry) = self.entries.entry(pubkey) else {
                // If this has happened the index is corrupted
                panic!("pubkey {pubkey} not found in cache index during remove");
            };
            let (_, ref_count) = occupied_entry.get_mut();
            *ref_count -= 1;
            if *ref_count == 0 {
                occupied_entry.remove_entry();
                self.num_unique_pubkeys.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Returns the recorded max slot for `pubkey`, or `None` if the pubkey is not present in the
    /// cache. Note: the account is not necessarily in this slot if it was removed during flush
    /// This is just the maximum slot that it could be found in during search
    fn max_slot_for_pubkey(&self, pubkey: &Pubkey) -> Option<Slot> {
        self.entries.get(pubkey).map(|entry| entry.0)
    }
}

#[derive(Debug, Default)]
pub struct AccountsCache {
    cache: DashMap<Slot, Arc<SlotCache>, BuildNoHashHasher<Slot>>,
    // Index to find which slots a pubkey is stored in, to speed up lookups in load_latest
    index: AccountsCacheIndex,
    // Queue of potentially unflushed roots. Random eviction + cache too large
    // could have triggered a flush of this slot already
    maybe_unflushed_roots: RwLock<BTreeSet<Slot>>,
    // Roots that have been claimed for flushing by `begin_flush_roots` but not yet
    // cleared by `end_flush_roots`
    roots_being_flushed: RwLock<BTreeSet<Slot>>,
    max_flushed_root: MaxFlushedRoot,
    /// The size of account data stored in the whole AccountsCache, in bytes
    total_size: Arc<AtomicU64>,
    /// The number of accounts stored in the whole AccountsCache
    total_accounts_counts: Arc<AtomicU64>,
}

impl AccountsCache {
    pub fn new_inner(&self) -> Arc<SlotCache> {
        Arc::new(SlotCache {
            cache: DashMap::default(),
            same_account_writes: AtomicU64::default(),
            same_account_writes_size: AtomicU64::default(),
            unique_account_writes_size: AtomicU64::default(),
            size: AtomicU64::default(),
            total_size: Arc::clone(&self.total_size),
            is_frozen: AtomicBool::default(),
            accounts_count: AtomicU64::new(0),
            total_accounts_count: Arc::clone(&self.total_accounts_counts),
        })
    }
    pub fn size(&self) -> u64 {
        self.total_size.load(Ordering::Relaxed)
    }
    pub fn report_size(&self) {
        datapoint_info!(
            "accounts_cache_size",
            (
                "num_roots",
                self.maybe_unflushed_roots.read().unwrap().len(),
                i64
            ),
            ("num_slots", self.cache.len(), i64),
            ("total_size", self.size(), i64),
            (
                "total_accounts_count",
                self.total_accounts_counts.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_unique_pubkeys",
                self.index.num_unique_pubkeys.load(Ordering::Relaxed),
                i64
            ),
        );
    }

    pub fn store(
        &self,
        slot: Slot,
        pubkey: &Pubkey,
        account: AccountSharedData,
    ) -> Arc<CachedAccount> {
        let slot_cache = self.slot_cache(slot).unwrap_or_else(||
            // DashMap entry.or_insert() returns a RefMut, essentially a write lock,
            // which is dropped after this block ends, minimizing time held by the lock.
            // However, we still want to persist the reference to the `SlotStores` behind
            // the lock, hence we clone it out, (`SlotStores` is an Arc so is cheap to clone).
            self
                .cache
                .entry(slot)
                .or_insert_with(|| self.new_inner())
                .clone());

        let (item, is_new_key) = slot_cache.insert(pubkey, account);
        if is_new_key {
            // Only update the index when the pubkey is new to this slot. Overwrites within the
            // same slot (is_new_key = false) cannot update the index because the ref count was
            // already incremented when the pubkey was first stored in this slot
            self.index.insert(pubkey, slot);
        }
        item
    }

    pub fn load(&self, slot: Slot, pubkey: &Pubkey) -> Option<Arc<CachedAccount>> {
        self.slot_cache(slot)
            .and_then(|slot_cache| slot_cache.get_cloned(pubkey))
    }

    pub fn remove_slot(&self, slot: Slot) -> Option<Arc<SlotCache>> {
        let result = self.cache.remove(&slot).map(|(_, slot_cache)| slot_cache);
        if let Some(slot_cache) = &result {
            self.index.remove(slot_cache.iter().map(|item| *item.key()));
        }
        result
    }

    /// Finds the best write-cache entry for `pubkey` visible from `ancestors`. Searches
    /// ancestors first (highest to lowest), then unflushed roots, then roots being flushed.
    /// Ancestors are checked exhaustively before roots, so a lower-slot ancestor wins over a
    /// higher-slot root. Returns the account, slot, and status, or `None` if not found.
    pub fn load_latest(
        &self,
        pubkey: &Pubkey,
        ancestors: &Ancestors,
    ) -> Option<(Arc<CachedAccount>, Slot, SlotStatus)> {
        // Exit early if the pubkey isn't in the cache
        let index_max_slot = self.index.max_slot_for_pubkey(pubkey)?;

        if let Some(ancestors_min_slot) = ancestors.min_slot() {
            // Iterate every slot in the range in descending order
            // Grab a read lock on flushing roots once before the loop to avoid locking/unlocking
            // on every iteration. This lock ensures that the load call in the loop correctly
            // identifies slots with status AncestorBeingFlushed.
            let r_roots_being_flushed = self.roots_being_flushed.read().unwrap();
            for slot in (ancestors_min_slot..=index_max_slot).rev() {
                if ancestors.contains_key(&slot) {
                    if let Some(account) = self.load(slot, pubkey) {
                        // Need to check flush status of the slot even for ancestors, because
                        // there could be newer version of the account that has already been
                        // flushed
                        let slot_status = if r_roots_being_flushed.contains(&slot) {
                            SlotStatus::AncestorBeingFlushed
                        } else {
                            SlotStatus::Ancestor
                        };
                        return Some((account, slot, slot_status));
                    }
                }
            }
            drop(r_roots_being_flushed);
        }

        // If the slot is not found in the ancestors fall back to searching roots
        let r_maybe_unflushed_roots = self.maybe_unflushed_roots.read().unwrap();
        for &slot in r_maybe_unflushed_roots.range(..=index_max_slot).rev() {
            if let Some(account) = self.load(slot, pubkey) {
                return Some((account, slot, SlotStatus::UnflushedRoot));
            }
        }
        drop(r_maybe_unflushed_roots);

        let r_roots_being_flushed = self.roots_being_flushed.read().unwrap();
        for &slot in r_roots_being_flushed.range(..=index_max_slot).rev() {
            if let Some(account) = self.load(slot, pubkey) {
                return Some((account, slot, SlotStatus::RootBeingFlushed));
            }
        }
        drop(r_roots_being_flushed);

        // Found nothing, the version of the account in the cache must be on a different fork
        None
    }

    pub fn slot_cache(&self, slot: Slot) -> Option<Arc<SlotCache>> {
        self.cache.get(&slot).map(|result| result.value().clone())
    }

    pub fn add_root(&self, root: Slot) {
        self.maybe_unflushed_roots.write().unwrap().insert(root);
    }

    pub fn num_unflushed_roots(&self) -> usize {
        self.maybe_unflushed_roots.read().unwrap().len()
    }

    /// Atomically moves roots up to and including `max_root` (or all roots if `None`) out of
    /// `maybe_unflushed_roots` and into `roots_being_flushed`, then returns them
    ///
    /// Panics if there are already roots being flushed (i.e. `end_flush_roots` was not called after
    /// a previous `begin_flush_roots`)
    pub fn begin_flush_roots(&self, max_root: Option<Slot>) -> BTreeSet<Slot> {
        let mut w_maybe_unflushed_roots = self.maybe_unflushed_roots.write().unwrap();
        let mut w_roots_being_flushed = self.roots_being_flushed.write().unwrap();

        assert!(
            w_roots_being_flushed.is_empty(),
            "begin_flush_roots called while roots are already being flushed; end_flush_roots must \
             be called first"
        );

        let roots_to_flush = if let Some(max_root) = max_root {
            let remaining = w_maybe_unflushed_roots.split_off(&(max_root + 1));
            std::mem::replace(&mut *w_maybe_unflushed_roots, remaining)
        } else {
            std::mem::take(&mut *w_maybe_unflushed_roots)
        };

        *w_roots_being_flushed = roots_to_flush.clone();
        roots_to_flush
    }

    /// Signals that flushing is complete. Clears the roots that were staged by `begin_flush_roots`
    pub fn end_flush_roots(&self) {
        self.roots_being_flushed.write().unwrap().clear();
    }

    pub fn cached_frozen_slots(&self) -> Vec<Slot> {
        self.cache
            .iter()
            .filter_map(|item| {
                let (slot, slot_cache) = item.pair();
                slot_cache.is_frozen().then_some(*slot)
            })
            .collect()
    }

    pub fn contains(&self, slot: Slot) -> bool {
        self.cache.contains_key(&slot)
    }

    pub fn num_slots(&self) -> usize {
        self.cache.len()
    }

    pub fn fetch_max_flush_root(&self) -> Option<Slot> {
        self.max_flushed_root.get()
    }

    pub fn set_max_flush_root(&self, root: Slot) {
        self.max_flushed_root.fetch_max(root);
    }
}

#[cfg(test)]
mod tests {
    use {super::*, test_case::test_case};

    impl AccountsCache {
        // Removes slots less than or equal to `max_root`. Only safe to pass in a rooted slot,
        // otherwise the slot removed could still be undergoing replay!
        pub fn remove_slots_le(&self, max_root: Slot) -> Vec<(Slot, Arc<SlotCache>)> {
            let mut removed_slots = vec![];
            self.cache.retain(|slot, slot_cache| {
                let should_remove = *slot <= max_root;
                if should_remove {
                    removed_slots.push((*slot, slot_cache.clone()))
                }
                !should_remove
            });
            removed_slots
        }
    }

    #[test]
    fn test_remove_slots_le() {
        let cache = AccountsCache::default();
        // Cache is empty, should return nothing
        assert!(cache.remove_slots_le(1).is_empty());
        let inserted_slot = 0;
        cache.store(
            inserted_slot,
            &Pubkey::new_unique(),
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        // If the cache is told the size limit is 0, it should return the one slot
        let removed = cache.remove_slots_le(0);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, inserted_slot);
    }

    #[test]
    fn test_cached_frozen_slots() {
        let cache = AccountsCache::default();
        // Cache is empty, should return nothing
        assert!(cache.cached_frozen_slots().is_empty());
        let inserted_slot = 0;
        cache.store(
            inserted_slot,
            &Pubkey::new_unique(),
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );

        // If the cache is told the size limit is 0, it should return nothing, because there's no
        // frozen slots
        assert!(cache.cached_frozen_slots().is_empty());
        cache.slot_cache(inserted_slot).unwrap().mark_slot_frozen();
        // If the cache is told the size limit is 0, it should return the one frozen slot
        assert_eq!(cache.cached_frozen_slots(), vec![inserted_slot]);
    }

    #[test]
    fn test_max_flushed_root_fetch_max() {
        let root = MaxFlushedRoot::default();
        assert_eq!(root.get(), None);

        root.fetch_max(0);
        assert_eq!(root.get(), Some(0));

        // first real slot replaces sentinel
        root.fetch_max(10);
        assert_eq!(root.get(), Some(10));

        // larger slot wins
        root.fetch_max(20);
        assert_eq!(root.get(), Some(20));

        // smaller and equal slots are ignored
        root.fetch_max(5);
        assert_eq!(root.get(), Some(20));
        root.fetch_max(20);
        assert_eq!(root.get(), Some(20));
    }

    #[test]
    fn test_cache_index_insert_and_max_slot() {
        let index = AccountsCacheIndex::default();
        let pubkey = Pubkey::new_unique();

        // Initially empty
        assert!(index.max_slot_for_pubkey(&pubkey).is_none());

        // Insert at slot 5
        index.insert(&pubkey, 5);
        assert_eq!(index.max_slot_for_pubkey(&pubkey), Some(5));

        // Insert same pubkey at a higher slot updates max_slot
        index.insert(&pubkey, 10);
        assert_eq!(index.max_slot_for_pubkey(&pubkey), Some(10));

        // Insert same pubkey at a lower slot does not decrease max_slot
        index.insert(&pubkey, 3);
        assert_eq!(index.max_slot_for_pubkey(&pubkey), Some(10));
    }

    #[test]
    fn test_cache_index_remove_decrements_count() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 0);

        // Store pubkey into 3 different slots
        cache.store(1, &pk, AccountSharedData::new(1, 0, &Pubkey::default()));
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);
        cache.store(5, &pk, AccountSharedData::new(5, 0, &Pubkey::default()));
        cache.store(3, &pk, AccountSharedData::new(3, 0, &Pubkey::default()));
        // Same pubkey across 3 slots — still only 1 unique pubkey
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);

        // Remove and drop slot 1 — entry should still exist (count goes from 3 to 2)
        let removed = cache.remove_slot(1);
        assert!(removed.is_some());
        drop(removed);
        assert_eq!(cache.index.max_slot_for_pubkey(&pk), Some(5));
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);

        // Remove and drop slot 5 — entry should still exist (count goes from 2 to 1)
        // max_slot stays stale at 5 because the index doesn't scan for a new max on removal
        let removed = cache.remove_slot(5);
        assert!(removed.is_some());
        drop(removed);
        assert!(cache.index.max_slot_for_pubkey(&pk).is_some());
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);

        // Remove and drop slot 3 — last reference gone, entry removed
        let removed = cache.remove_slot(3);
        assert!(removed.is_some());
        drop(removed);
        assert!(cache.index.max_slot_for_pubkey(&pk).is_none());
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_remove_slot_cleans_up_index() {
        let cache = AccountsCache::default();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();

        // pk1 in slots 1 and 3; pk2 only in slot 1
        cache.store(1, &pk1, AccountSharedData::new(1, 0, &Pubkey::default()));
        cache.store(1, &pk2, AccountSharedData::new(1, 0, &Pubkey::default()));
        cache.store(3, &pk1, AccountSharedData::new(1, 0, &Pubkey::default()));

        // Before removal: both pubkeys are in the index
        assert!(cache.index.max_slot_for_pubkey(&pk1).is_some());
        assert!(cache.index.max_slot_for_pubkey(&pk2).is_some());

        // Remove slot 1 — pk2 should disappear, pk1 still present (in slot 3)
        cache.remove_slot(1);
        assert!(cache.index.max_slot_for_pubkey(&pk1).is_some());
        assert!(cache.index.max_slot_for_pubkey(&pk2).is_none());

        // Remove slot 3 — pk1 should also disappear
        cache.remove_slot(3);
        assert!(cache.index.max_slot_for_pubkey(&pk1).is_none());
    }

    /// Tests that `load_latest` returns the correct slot, status, and account value
    /// given various combinations of ancestor slots, root slots, and flushing state.
    ///
    /// Ancestors always take priority over roots regardless of slot
    // None case
    #[test_case(&[], &[], &[], None; "not ancestor not root")]
    #[test_case(&[10], &[], &[], Some((10, SlotStatus::Ancestor)); "ancestor only")]
    #[test_case(&[5, 10, 15], &[], &[], Some((15, SlotStatus::Ancestor)); "highest ancestor returned")]
    #[test_case(&[], &[10, 20], &[], Some((20, SlotStatus::UnflushedRoot)); "rooted, with no ancestors")]
    #[test_case(&[], &[], &[10], Some((10, SlotStatus::RootBeingFlushed)); "root being flushed")]
    #[test_case(&[10], &[], &[10], Some((10, SlotStatus::AncestorBeingFlushed)); "ancestor being flushed")]
    #[test_case(&[5], &[20], &[], Some((5, SlotStatus::Ancestor)); "ancestor wins over higher root")]
    #[test_case(&[], &[20], &[10], Some((20, SlotStatus::UnflushedRoot));"unflushed root over flushing root")]
    #[test_case(&[5], &[20], &[10], Some((5, SlotStatus::Ancestor));"ancestor over unflushed and flushing roots")]
    fn test_load_latest_slot_priority(
        ancestor_slots: &[Slot],
        unflushed_root_slots: &[Slot],
        flushing_root_slots: &[Slot],
        expected: Option<(Slot, SlotStatus)>,
    ) {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        for &slot in ancestor_slots {
            cache.store(
                slot,
                &pk,
                AccountSharedData::new(slot, 0, &Pubkey::default()),
            );
        }
        for &slot in unflushed_root_slots.iter().chain(flushing_root_slots) {
            cache.store(
                slot,
                &pk,
                AccountSharedData::new(slot, 0, &Pubkey::default()),
            );
            cache.add_root(slot);
        }
        if let Some(&max) = flushing_root_slots.iter().max() {
            cache.begin_flush_roots(Some(max));
        }

        let ancestors = Ancestors::from(ancestor_slots.to_vec());
        let result = cache
            .load_latest(&pk, &ancestors)
            .map(|(account, slot, status)| {
                assert_eq!(account.account.lamports(), slot);
                (slot, status)
            });
        assert_eq!(result, expected);
    }

    #[test]
    fn test_load_latest_ignores_non_ancestor_non_root_slot() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        // Store an account at slot 10, but don't add it as an ancestor or root.
        cache.store(10, &pk, AccountSharedData::new(10, 0, &Pubkey::default()));

        let ancestors = Ancestors::from(vec![5, 15]);
        let result = cache.load_latest(&pk, &ancestors);
        assert!(result.is_none());
    }

    #[test]
    fn test_visibility_after_flush() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        cache.store(10, &pk, AccountSharedData::new(100, 0, &Pubkey::default()));
        cache.add_root(10);
        cache.begin_flush_roots(None);
        cache.end_flush_roots();

        // After clearing flushed roots, slot is no longer visible
        let empty = Ancestors::default();
        assert!(cache.load_latest(&pk, &empty).is_none());
    }

    #[test]
    fn test_begin_flush_roots_with_max_root() {
        let cache = AccountsCache::default();
        cache.add_root(1);
        cache.add_root(3);
        cache.add_root(5);
        cache.add_root(7);

        // begin_flush_roots(Some(5)) should return {1, 3, 5} and leave {7}
        let taken = cache.begin_flush_roots(Some(5));
        assert_eq!(taken, BTreeSet::from([1, 3, 5]));

        // Remaining unflushed roots should only contain 7
        assert_eq!(cache.maybe_unflushed_roots.read().unwrap().len(), 1);
        assert!(cache.maybe_unflushed_roots.read().unwrap().contains(&7));

        // Taken roots should now be in roots_being_flushed
        assert!(cache.roots_being_flushed.read().unwrap().contains(&1));
        assert!(cache.roots_being_flushed.read().unwrap().contains(&3));
        assert!(cache.roots_being_flushed.read().unwrap().contains(&5));
        assert!(!cache.roots_being_flushed.read().unwrap().contains(&7));

        cache.end_flush_roots();
    }

    #[test]
    fn test_begin_flush_roots_none_takes_all() {
        let cache = AccountsCache::default();
        cache.add_root(2);
        cache.add_root(4);
        cache.add_root(6);

        let taken = cache.begin_flush_roots(None);
        assert_eq!(taken, BTreeSet::from([2, 4, 6]));

        // All unflushed roots should be drained
        assert!(cache.maybe_unflushed_roots.read().unwrap().is_empty());

        // All should be in roots_being_flushed
        assert_eq!(
            *cache.roots_being_flushed.read().unwrap(),
            BTreeSet::from([2, 4, 6])
        );

        cache.end_flush_roots();
    }

    #[test]
    #[should_panic(expected = "begin_flush_roots called while roots are already being flushed")]
    fn test_begin_flush_roots_panics_if_already_flushing() {
        let cache = AccountsCache::default();
        cache.add_root(1);
        cache.begin_flush_roots(None);
        // Calling again without end_flush_roots should panic
        cache.add_root(2);
        cache.begin_flush_roots(None);
    }

    #[test]
    fn test_end_flush_roots_allows_next_flush() {
        let cache = AccountsCache::default();
        cache.add_root(1);
        cache.add_root(2);

        // First cycle
        let taken = cache.begin_flush_roots(None);
        assert_eq!(taken, BTreeSet::from([1, 2]));

        cache.end_flush_roots();

        // After clear, roots_being_flushed is empty
        assert!(cache.roots_being_flushed.read().unwrap().is_empty());

        // Second cycle works without panic
        cache.add_root(3);
        let taken = cache.begin_flush_roots(None);
        assert_eq!(taken, BTreeSet::from([3]));
        cache.end_flush_roots();
    }

    #[test]
    fn test_load_latest_multiple_ancestors_one_being_flushed() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        // Store at slots 5 and 10, both will be ancestors
        cache.store(5, &pk, AccountSharedData::new(5, 0, &Pubkey::default()));
        cache.store(10, &pk, AccountSharedData::new(10, 0, &Pubkey::default()));

        // Slot 10 is also a root that gets claimed for flushing
        cache.add_root(10);
        cache.begin_flush_roots(None);

        let ancestors = Ancestors::from(vec![5, 10]);
        let (account, slot, status) = cache.load_latest(&pk, &ancestors).unwrap();

        // Slot 10 is highest ancestor but is being flushed, so should return AncestorBeingFlushed
        assert_eq!(slot, 10);
        assert_eq!(status, SlotStatus::AncestorBeingFlushed);
        assert_eq!(account.account.lamports(), 10);

        cache.end_flush_roots();
    }
}
