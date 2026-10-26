use {
    crate::{ancestors::Ancestors, bank_id::BankId},
    dashmap::{DashMap, mapref::entry::Entry},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_nohash_hasher::{BuildNoHashHasher, IntMap},
    solana_pubkey::Pubkey,
    std::{
        collections::{BTreeMap, BTreeSet},
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
    slot: Slot,
    cache: DashMap<Pubkey, Arc<CachedAccount>, ahash::RandomState>,
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

    pub fn slot(&self) -> Slot {
        self.slot
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
    type Target = DashMap<Pubkey, Arc<CachedAccount>, ahash::RandomState>;
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

/// Maps each pubkey to (max_bank_id, ref_count) where max_bank_id is the highest bank id that has
/// written the pubkey into the cache, and ref_count is the number of SlotCache entries that
/// currently hold the pubkey. max_bank_id may be stale after a removal; callers must handle a
/// look-up miss on max_bank_id by falling back to scanning all slots in the cache (see load_latest)
#[derive(Debug, Default)]
struct AccountsCacheIndex {
    entries: DashMap<Pubkey, (BankId, u32), ahash::RandomState>,
    // The number of unique pubkeys in the index, for reporting purposes. This is to avoid having to
    // lock each shard of the entries dashmap to count unique keys on demand
    num_unique_pubkeys: AtomicU64,
}

impl AccountsCacheIndex {
    /// Inserts an entry into the index. If the entry is already present, increase the ref count
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId) {
        self.entries
            .entry(*pubkey)
            .and_modify(|(stored_bank_id, ref_count)| {
                *stored_bank_id = bank_id.max(*stored_bank_id);
                *ref_count += 1;
            })
            .or_insert_with(|| {
                self.num_unique_pubkeys.fetch_add(1, Ordering::Relaxed);
                (bank_id, 1)
            });
    }

    /// Decrement the reference count for each pubkey in `pubkeys`. Removes an entry entirely if
    /// the count reaches zero. `max_bank_id` is not updated; it will become stale if the removed
    /// bank is the highest. Returns a vec of pubkeys removed from the index.
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

    /// Returns the highest bank id that stored `pubkey` in the cache, or `None` if the pubkey is
    /// not present in the cache. That bank's slot cache may since have been flushed or purged, so
    /// this is only an upper bound on the banks whose slot caches hold `pubkey`
    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId> {
        self.entries.get(pubkey).map(|entry| entry.0)
    }
}

#[derive(Debug, Default)]
pub struct AccountsCache {
    cache: DashMap<BankId, Arc<SlotCache>, BuildNoHashHasher<BankId>>,
    /// Maps a slot to the id of the bank that stored accounts at it
    bank_ids: RwLock<IntMap<Slot, BankId>>,
    // Index to find which banks a pubkey is stored in, to speed up lookups in load_latest
    index: AccountsCacheIndex,
    // Rooted banks that still have accounts in the write cache, and the slot each stored at. A
    // bank enters via `add_root` if it has a slot cache, and is dropped by `remove_slot` when its
    // slot cache is flushed. Roots form one chain, so bank id order is slot order.
    unflushed_roots: RwLock<BTreeMap<BankId, Slot>>,
    max_flushed_root: MaxFlushedRoot,
    /// The size of account data stored in the whole AccountsCache, in bytes
    total_size: Arc<AtomicU64>,
    /// The number of accounts stored in the whole AccountsCache
    total_accounts_count: Arc<AtomicU64>,
}

impl AccountsCache {
    pub fn new_inner(&self, slot: Slot) -> Arc<SlotCache> {
        Arc::new(SlotCache {
            slot,
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
        bank_id: BankId,
        pubkey: &Pubkey,
        account: AccountSharedData,
    ) -> Arc<CachedAccount> {
        let slot_cache = self.bank_cache(bank_id).unwrap_or_else(|| {
            let slot_bank_id = *self
                .bank_ids
                .write()
                .unwrap()
                .entry(slot)
                .or_insert(bank_id);
            debug_assert_eq!(
                slot_bank_id, bank_id,
                "slot {slot} already holds accounts stored by another bank"
            );
            // DashMap entry.or_insert() returns a RefMut, essentially a write lock,
            // which is dropped after this block ends, minimizing time held by the lock.
            // However, we still want to persist the reference to the `SlotStores` behind
            // the lock, hence we clone it out, (`SlotStores` is an Arc so is cheap to clone).
            self.cache
                .entry(bank_id)
                .or_insert_with(|| self.new_inner(slot))
                .clone()
        });
        debug_assert_eq!(slot_cache.slot, slot, "{bank_id:?} stores at another slot");

        let (item, is_new_key) = slot_cache.insert(pubkey, account);
        if is_new_key {
            // Only update the index when the pubkey is new to this slot. Overwrites within the
            // same slot (is_new_key = false) cannot update the index because the ref count was
            // already incremented when the pubkey was first stored in this slot
            self.index.insert(pubkey, bank_id);
        }
        item
    }

    /// Returns `pubkey`'s account from `bank_id`'s cache, and the slot it was stored at
    pub fn load(&self, bank_id: BankId, pubkey: &Pubkey) -> Option<(Arc<CachedAccount>, Slot)> {
        let slot_cache = self.bank_cache(bank_id)?;
        let account = slot_cache.get_cloned(pubkey)?;
        Some((account, slot_cache.slot))
    }

    /// Removes a slot from the accounts cache and returns the set of pubkeys removed from the index.
    #[must_use]
    pub fn remove_slot(&self, slot: Slot) -> Option<Vec<Pubkey>> {
        let slot_cache = {
            // Held across the cache removal so a concurrent `add_root` can't re-track this bank
            let mut unflushed_roots = self.unflushed_roots.write().unwrap();
            let bank_id = self.bank_ids.write().unwrap().remove(&slot)?;
            // If this bank was a root, it has now left the cache, so stop tracking it as unflushed.
            unflushed_roots.remove(&bank_id);
            self.cache.remove(&bank_id)?.1
        };
        Some(self.index.remove(slot_cache.iter().map(|item| *item.key())))
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
        let index_max_bank_id = self.index.max_bank_id_for_pubkey(pubkey)?;

        // Ancestors take priority over roots regardless of slot. Walk them in descending order,
        // skipping those above the newest cached version, and return the first (highest) that has it.
        for bank_id in ancestors
            .iter_bank_ids()
            .rev()
            .skip_while(|bank_id| *bank_id > index_max_bank_id)
        {
            if let Some(found) = self.load(bank_id, pubkey) {
                return Some(found);
            }
        }

        // If the slot is not found in the ancestors fall back to searching roots.
        // Bound the search to the oldest ancestor so that roots beyond the querying
        // bank's ancestor chain are not visible. Using the oldest is more correct than
        // the newest because a root between them that is not an ancestor belongs to a
        // different fork and should not be returned.
        let max_root_bank_id = ancestors
            .iter_bank_ids()
            .next()
            .map_or(index_max_bank_id, |bank_id| bank_id.min(index_max_bank_id));

        let r_unflushed_roots = self.unflushed_roots.read().unwrap();
        for &bank_id in r_unflushed_roots
            .range(..=max_root_bank_id)
            .rev()
            .map(|(bank_id, _slot)| bank_id)
        {
            if let Some(found) = self.load(bank_id, pubkey) {
                return Some(found);
            }
        }
        drop(r_unflushed_roots);

        // Found nothing, the version of the account in the cache must be on a different fork
        None
    }

    /// Returns the cache of the bank that stored accounts at `slot`
    pub fn slot_cache(&self, slot: Slot) -> Option<Arc<SlotCache>> {
        let bank_id = self.bank_id(slot)?;
        self.bank_cache(bank_id)
    }

    /// Returns `bank_id`'s cache
    pub fn bank_cache(&self, bank_id: BankId) -> Option<Arc<SlotCache>> {
        self.cache
            .get(&bank_id)
            .map(|result| result.value().clone())
    }

    /// Returns the id of the bank that stored accounts at `slot`
    pub fn bank_id(&self, slot: Slot) -> Option<BankId> {
        self.bank_ids.read().unwrap().get(&slot).copied()
    }

    /// A root without a slot cache is not tracked; nothing stores into a slot once it is rooted
    pub fn add_root(&self, bank_id: BankId) -> Option<Slot> {
        // Held across the lookup so a concurrent `remove_slot` can't leave this root stranded
        let mut unflushed_roots = self.unflushed_roots.write().unwrap();
        if let Some(slot) = self.cache.get(&bank_id).map(|slot_cache| slot_cache.slot) {
            unflushed_roots.insert(bank_id, slot);
            Some(slot)
        } else {
            None
        }
    }

    pub fn num_unflushed_roots(&self) -> usize {
        self.unflushed_roots.read().unwrap().len()
    }

    /// Returns whether `slot` is a root that has been added but not yet flushed to storage.
    pub fn contains_unflushed_root(&self, slot: Slot) -> bool {
        self.bank_id(slot)
            .is_some_and(|bank_id| self.unflushed_roots.read().unwrap().contains_key(&bank_id))
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
            .values()
            .take_while(|slot| **slot <= max_root)
            .copied()
            .collect()
    }

    pub fn cached_frozen_slots(&self) -> Vec<Slot> {
        self.cache
            .iter()
            .filter_map(|item| {
                let slot_cache = item.value();
                slot_cache.is_frozen().then_some(slot_cache.slot)
            })
            .collect()
    }

    pub fn contains(&self, slot: Slot) -> bool {
        self.bank_ids.read().unwrap().contains_key(&slot)
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
            self.cache.retain(|_bank_id, slot_cache| {
                let should_remove = slot_cache.slot <= max_root;
                if should_remove {
                    removed_slots.push((slot_cache.slot, slot_cache.clone()))
                }
                !should_remove
            });
            self.bank_ids
                .write()
                .unwrap()
                .retain(|slot, _bank_id| *slot > max_root);
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
            BankId::new(inserted_slot),
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
            BankId::new(inserted_slot),
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
            BankId::new(slot),
            &pubkey,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        let slot_cache = cache.slot_cache(slot).unwrap();
        assert_eq!(slot_cache.len(), 1);

        cache.store(
            slot,
            BankId::new(slot),
            &pubkey,
            AccountSharedData::new(2, 0, &Pubkey::default()),
        );
        assert_eq!(slot_cache.len(), 1);

        cache.store(
            slot,
            BankId::new(slot),
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
    fn test_cache_index_insert_and_max_bank_id() {
        let index = AccountsCacheIndex::default();
        let pubkey = Pubkey::new_unique();

        // Initially empty
        assert!(index.max_bank_id_for_pubkey(&pubkey).is_none());

        // Insert from bank 5
        index.insert(&pubkey, BankId::new(5));
        assert_eq!(index.max_bank_id_for_pubkey(&pubkey), Some(BankId::new(5)));

        // Insert same pubkey from a higher bank id updates max_bank_id
        index.insert(&pubkey, BankId::new(10));
        assert_eq!(index.max_bank_id_for_pubkey(&pubkey), Some(BankId::new(10)));

        // Insert same pubkey from a lower bank id does not decrease max_bank_id
        index.insert(&pubkey, BankId::new(3));
        assert_eq!(index.max_bank_id_for_pubkey(&pubkey), Some(BankId::new(10)));
    }

    #[test]
    fn test_cache_index_remove_decrements_count() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 0);

        // Store pubkey into 3 different slots
        cache.store(
            1,
            BankId::new(1),
            &pk,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);
        cache.store(
            5,
            BankId::new(5),
            &pk,
            AccountSharedData::new(5, 0, &Pubkey::default()),
        );
        cache.store(
            3,
            BankId::new(3),
            &pk,
            AccountSharedData::new(3, 0, &Pubkey::default()),
        );
        // Same pubkey across 3 slots — still only 1 unique pubkey
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);

        // Remove and drop slot 1 — entry should still exist (count goes from 3 to 2)
        let removed = cache.remove_slot(1);
        assert!(removed.is_some());
        drop(removed);
        assert_eq!(
            cache.index.max_bank_id_for_pubkey(&pk),
            Some(BankId::new(5))
        );
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);

        // Remove and drop slot 5 — entry should still exist (count goes from 2 to 1)
        // max_bank_id stays stale at 5 because the index doesn't scan for a new max on removal
        let removed = cache.remove_slot(5);
        assert!(removed.is_some());
        drop(removed);
        assert!(cache.index.max_bank_id_for_pubkey(&pk).is_some());
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 1);

        // Remove and drop slot 3 — last reference gone, entry removed
        let removed = cache.remove_slot(3);
        assert!(removed.is_some());
        drop(removed);
        assert!(cache.index.max_bank_id_for_pubkey(&pk).is_none());
        assert_eq!(cache.index.num_unique_pubkeys.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_remove_slot_cleans_up_index() {
        let cache = AccountsCache::default();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();

        // pk1 in slots 1 and 3; pk2 only in slot 1
        cache.store(
            1,
            BankId::new(1),
            &pk1,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        cache.store(
            1,
            BankId::new(1),
            &pk2,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        cache.store(
            3,
            BankId::new(3),
            &pk1,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );

        // Before removal: both pubkeys are in the index
        assert!(cache.index.max_bank_id_for_pubkey(&pk1).is_some());
        assert!(cache.index.max_bank_id_for_pubkey(&pk2).is_some());

        // Remove slot 1 — pk2 should disappear, pk1 still present (in slot 3)
        let pubkeys_removed = cache.remove_slot(1);
        assert_eq!(pubkeys_removed, Some(vec![pk2]));
        assert!(cache.index.max_bank_id_for_pubkey(&pk1).is_some());
        assert!(cache.index.max_bank_id_for_pubkey(&pk2).is_none());

        // Remove slot 3 — pk1 should also disappear
        let pubkeys_removed = cache.remove_slot(3);
        assert_eq!(pubkeys_removed, Some(vec![pk1]));
        assert!(cache.index.max_bank_id_for_pubkey(&pk1).is_none());
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
                BankId::new(slot),
                &pk,
                AccountSharedData::new(slot, 0, &Pubkey::default()),
            );
        }
        for &slot in root_slots {
            cache.store(
                slot,
                BankId::new(slot),
                &pk,
                AccountSharedData::new(slot, 0, &Pubkey::default()),
            );
            cache.add_root(BankId::new(slot));
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
        cache.store(
            10,
            BankId::new(10),
            &pk,
            AccountSharedData::new(10, 0, &Pubkey::default()),
        );

        let ancestors = Ancestors::from(vec![5, 15]);
        let result = cache.load_latest(&pk, &ancestors);
        assert!(result.is_none());
    }

    #[test]
    fn test_visibility_after_flush() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();

        cache.store(
            10,
            BankId::new(10),
            &pk,
            AccountSharedData::new(100, 0, &Pubkey::default()),
        );
        cache.add_root(BankId::new(10));
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
        for slot in [1, 3, 5, 7] {
            cache.store(
                slot,
                BankId::new(slot),
                &Pubkey::new_unique(),
                AccountSharedData::new(1, 0, &Pubkey::default()),
            );
            cache.add_root(BankId::new(slot));
        }

        // roots_to_flush(Some(5)) returns {1, 3, 5}, excluding 7.
        let to_flush = cache.roots_to_flush(Some(5));
        assert_eq!(to_flush, BTreeSet::from([1, 3, 5]));

        // roots_to_flush only reads: the tracked roots are unchanged.
        assert_eq!(cache.roots_to_flush(None), BTreeSet::from([1, 3, 5, 7]));
    }

    #[test]
    fn test_roots_to_flush_none_takes_all() {
        let cache = AccountsCache::default();
        for slot in [2, 4, 6] {
            cache.store(
                slot,
                BankId::new(slot),
                &Pubkey::new_unique(),
                AccountSharedData::new(1, 0, &Pubkey::default()),
            );
            cache.add_root(BankId::new(slot));
        }

        let to_flush = cache.roots_to_flush(None);
        assert_eq!(to_flush, BTreeSet::from([2, 4, 6]));

        // roots_to_flush only reads: the tracked roots are unchanged.
        assert_eq!(cache.roots_to_flush(None), BTreeSet::from([2, 4, 6]));
    }

    #[test]
    fn test_remove_slot_drops_unflushed_root() {
        let cache = AccountsCache::default();
        let pk = Pubkey::new_unique();
        cache.store(
            1,
            BankId::new(1),
            &pk,
            AccountSharedData::new(1, 0, &Pubkey::default()),
        );
        cache.add_root(BankId::new(1));
        cache.store(
            2,
            BankId::new(2),
            &pk,
            AccountSharedData::new(2, 0, &Pubkey::default()),
        );
        cache.add_root(BankId::new(2));

        // remove_slot drops slot 1 from both the cache and the tracked roots, leaving slot 2.
        let _ = cache.remove_slot(1);
        assert_eq!(cache.roots_to_flush(None), BTreeSet::from([2]));
    }
}
