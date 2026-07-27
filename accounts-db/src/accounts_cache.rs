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
    pub fn len(&self) -> usize {
        self.accounts_count.load(Ordering::Acquire) as usize
    }

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
            self.accounts_count.fetch_add(1, Ordering::Release);
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
    /// is the highest slot. Returns a vec of pubkeys removed from the index.
    fn remove(&self, pubkeys: impl IntoIterator<Item = Pubkey>) -> Vec<Pubkey> {
        let mut removed_pubkeys = Vec::new();
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
                removed_pubkeys.push(pubkey);
            }
        }
        removed_pubkeys
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
    // Rooted slots that may still have accounts in the write cache. A slot enters via `add_root`
    // and is dropped either by `remove_slot` when its cache is flushed, or by
    // `remove_unflushed_root` when a flush finds it had no cache.
    unflushed_roots: RwLock<BTreeSet<Slot>>,
    max_flushed_root: MaxFlushedRoot,
    /// The size of account data stored in the whole AccountsCache, in bytes
    total_size: Arc<AtomicU64>,
    /// The number of accounts stored in the whole AccountsCache
    total_accounts_count: Arc<AtomicU64>,
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
            total_accounts_count: Arc::clone(&self.total_accounts_count),
        })
    }
    pub fn size(&self) -> u64 {
        self.total_size.load(Ordering::Relaxed)
    }
    pub fn report_size(&self) {
        datapoint_info!(
            "accounts_cache_size",
            ("num_roots", self.unflushed_roots.read().unwrap().len(), i64),
            ("num_slots", self.cache.len(), i64),
            ("total_size", self.size(), i64),
            (
                "total_accounts_count",
                self.total_accounts_count.load(Ordering::Relaxed),
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

    /// Removes a slot from the accounts cache and returns the set of pubkeys removed from the index.
    #[must_use]
    pub fn remove_slot(&self, slot: Slot) -> Option<Vec<Pubkey>> {
        let result = self.cache.remove(&slot).map(|(_, slot_cache)| slot_cache);
        // If this slot was a root, it has now left the cache, so stop tracking it as unflushed.
        self.unflushed_roots.write().unwrap().remove(&slot);

        result
            .as_ref()
            .map(|slot_cache| self.index.remove(slot_cache.iter().map(|item| *item.key())))
    }

    /// Finds the newest write-cache entry for `pubkey` visible from `ancestors`. Searches
    /// ancestors first (highest to lowest), then roots (highest to lowest). Ancestors are
    /// checked exhaustively before roots, so a lower-slot ancestor wins over a higher-slot
    /// root. Returns the account and its slot, or `None` if not found.
    pub fn load_latest(
        &self,
        pubkey: &Pubkey,
        ancestors: &Ancestors,
    ) -> Option<(Arc<CachedAccount>, Slot)> {
        // Exit early if the pubkey isn't in the cache
        let index_max_slot = self.index.max_slot_for_pubkey(pubkey)?;

        // Ancestors take priority over roots regardless of slot. Iterate every slot in the
        // range in descending order and return the first (highest) ancestor that has it.
        if let Some(ancestors_min_slot) = ancestors.min_slot() {
            // Bound the search to ancestors.max_slot() as slots > than ancestors max_slot
            // are not visible to the querying bank.
            let max_slot = ancestors.max_slot().min(index_max_slot);
            for slot in (ancestors_min_slot..=max_slot).rev() {
                if ancestors.contains_key(&slot)
                    && let Some(account) = self.load(slot, pubkey)
                {
                    return Some((account, slot));
                }
            }
        }

        // If the slot is not found in the ancestors fall back to searching roots.
        // Bound the search to ancestors.min_slot() so that roots from slots beyond
        // the querying bank's ancestor chain are not visible. Using min_slot is more
        // correct than max_slot because a root between min and max that is not an
        // ancestor belongs to a different fork and should not be returned.
        let max_root_slot = ancestors
            .min_slot()
            .unwrap_or(index_max_slot)
            .min(index_max_slot);

        let r_unflushed_roots = self.unflushed_roots.read().unwrap();
        for &slot in r_unflushed_roots.range(..=max_root_slot).rev() {
            if let Some(account) = self.load(slot, pubkey) {
                return Some((account, slot));
            }
        }
        drop(r_unflushed_roots);

        // Found nothing, the version of the account in the cache must be on a different fork
        None
    }

    pub fn slot_cache(&self, slot: Slot) -> Option<Arc<SlotCache>> {
        self.cache.get(&slot).map(|result| result.value().clone())
    }

    pub fn add_root(&self, root: Slot) {
        self.unflushed_roots.write().unwrap().insert(root);
    }

    pub fn num_unflushed_roots(&self) -> usize {
        self.unflushed_roots.read().unwrap().len()
    }

    /// Returns whether `slot` is a root that has been added but not yet flushed to storage.
    pub fn contains_unflushed_root(&self, slot: Slot) -> bool {
        self.unflushed_roots.read().unwrap().contains(&slot)
    }

    /// Returns the unflushed roots up to and including `max_root` (or all roots if `None`). The
    /// returned roots remain tracked as unflushed until `remove_slot` drops each one when its cache
    /// is flushed.
    pub fn roots_to_flush(&self, max_root: Option<Slot>) -> BTreeSet<Slot> {
        // `None` means no upper bound, i.e. every root.
        let max_root = max_root.unwrap_or(Slot::MAX);
        self.unflushed_roots
            .read()
            .unwrap()
            .range(..=max_root)
            .copied()
            .collect()
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

    pub fn contains_pubkey(&self, pubkey: &Pubkey) -> bool {
        self.index.entries.contains_key(pubkey)
    }

    /// Returns a vector of all pubkeys currently in the cache index.
    /// In iterator is not returned as the dashmap shards would be readlocked for the duration
    /// of the iterator
    pub(crate) fn cached_pubkeys(&self) -> Vec<Pubkey> {
        self.index
            .entries
            .iter()
            .map(|entry| *entry.key())
            .collect()
    }

    pub fn num_slots(&self) -> usize {
        self.cache.len()
    }

    pub fn fetch_max_flush_root(&self) -> Option<Slot> {
        self.max_flushed_root.get()
    }

    /// Stop tracking `slot` as an unflushed root without touching its cache. Used when a flushed
    /// root had no cache for `remove_slot` to drop (e.g. genesis), so it would otherwise linger.
    pub fn remove_unflushed_root(&self, slot: Slot) {
        self.unflushed_roots.write().unwrap().remove(&slot);
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
    fn test_slot_cache_len_tracks_unique_accounts() {
        let cache = AccountsCache::default();
        let slot = 0;
        let pubkey = Pubkey::new_unique();
        let other_pubkey = Pubkey::new_unique();

        cache.store(
            slot,
            &pubkey,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        let slot_cache = cache.slot_cache(slot).unwrap();
        assert_eq!(slot_cache.len(), 1);

        cache.store(
            slot,
            &pubkey,
            AccountSharedData::new(2, 0, &Pubkey::default()),
        );
        assert_eq!(slot_cache.len(), 1);

        cache.store(
            slot,
            &other_pubkey,
            AccountSharedData::new(3, 0, &Pubkey::default()),
        );
        assert_eq!(slot_cache.len(), 2);
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
        let pubkeys_removed = cache.remove_slot(1);
        assert_eq!(pubkeys_removed, Some(vec![pk2]));
        assert!(cache.index.max_slot_for_pubkey(&pk1).is_some());
        assert!(cache.index.max_slot_for_pubkey(&pk2).is_none());

        // Remove slot 3 — pk1 should also disappear
        let pubkeys_removed = cache.remove_slot(3);
        assert_eq!(pubkeys_removed, Some(vec![pk1]));
        assert!(cache.index.max_slot_for_pubkey(&pk1).is_none());
    }

    /// Tests that `load_latest` returns the correct slot and account value
    /// given various combinations of ancestor slots and root slots.
    ///
    /// Ancestors always take priority over roots regardless of slot
    // None case
    // `uncached_ancestors` are slots added to the Ancestors set but with no account
    // data stored in the cache. This lets us test root bounding by min_slot
    // without the ancestor path short-circuiting the lookup.
    #[test_case(&[], &[], &[], None; "not ancestor not root")]
    #[test_case(&[10], &[], &[], Some(10); "ancestor only")]
    #[test_case(&[5, 10, 15], &[], &[], Some(15); "highest ancestor returned")]
    #[test_case(&[], &[10, 20], &[], Some(20); "rooted, with no ancestors")]
    #[test_case(&[5], &[20], &[], Some(5); "ancestor wins over higher root")]
    // Root beyond ancestors.min_slot() is excluded; older root still found
    #[test_case(&[], &[5, 11], &[10], Some(5); "root beyond min ancestor excluded")]
    // Root within min_slot bound is still returned
    #[test_case(&[], &[10], &[15], Some(10); "root below min ancestor returned")]
    fn test_load_latest_slot_priority(
        ancestor_slots: &[Slot],
        root_slots: &[Slot],
        uncached_ancestors: &[Slot],
        expected: Option<Slot>,
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
        for &slot in root_slots {
            cache.store(
                slot,
                &pk,
                AccountSharedData::new(slot, 0, &Pubkey::default()),
            );
            cache.add_root(slot);
        }

        let mut all_ancestors: Vec<Slot> = ancestor_slots.to_vec();
        all_ancestors.extend_from_slice(uncached_ancestors);
        let ancestors = Ancestors::from(all_ancestors);
        let result = cache.load_latest(&pk, &ancestors).map(|(account, slot)| {
            assert_eq!(account.account.lamports(), slot);
            slot
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
        // A flush finishes a slot with `remove_slot`, which drops both its cache and its
        // unflushed-root tracking; call it directly here to stand in for that flush.
        let _ = cache.remove_slot(10);

        // With the slot gone from the cache and untracked as a root, it is not visible.
        let empty = Ancestors::default();
        assert!(cache.load_latest(&pk, &empty).is_none());
        assert!(cache.unflushed_roots.read().unwrap().is_empty());
    }

    #[test]
    fn test_roots_to_flush_with_max_root() {
        let cache = AccountsCache::default();
        cache.add_root(1);
        cache.add_root(3);
        cache.add_root(5);
        cache.add_root(7);

        // roots_to_flush(Some(5)) returns {1, 3, 5}, excluding 7.
        let to_flush = cache.roots_to_flush(Some(5));
        assert_eq!(to_flush, BTreeSet::from([1, 3, 5]));

        // roots_to_flush only reads: the tracked roots are unchanged.
        assert_eq!(
            *cache.unflushed_roots.read().unwrap(),
            BTreeSet::from([1, 3, 5, 7])
        );
    }

    #[test]
    fn test_roots_to_flush_none_takes_all() {
        let cache = AccountsCache::default();
        cache.add_root(2);
        cache.add_root(4);
        cache.add_root(6);

        let to_flush = cache.roots_to_flush(None);
        assert_eq!(to_flush, BTreeSet::from([2, 4, 6]));

        // roots_to_flush only reads: the tracked roots are unchanged.
        assert_eq!(
            *cache.unflushed_roots.read().unwrap(),
            BTreeSet::from([2, 4, 6])
        );
    }

    #[test]
    fn test_remove_slot_drops_unflushed_root() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();
        cache.store(1, &pk, AccountSharedData::new(1, 0, &Pubkey::default()));
        cache.add_root(1);
        cache.store(2, &pk, AccountSharedData::new(2, 0, &Pubkey::default()));
        cache.add_root(2);

        // remove_slot drops slot 1 from both the cache and the tracked roots, leaving slot 2.
        let _ = cache.remove_slot(1);
        assert_eq!(*cache.unflushed_roots.read().unwrap(), BTreeSet::from([2]));
    }
}
