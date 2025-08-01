use {
    ahash::RandomState as AHashRandomState,
    dashmap::DashMap,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_nohash_hasher::BuildNoHashHasher,
    solana_pubkey::Pubkey,
    std::{
        collections::BTreeSet,
        ops::Deref,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
    },
};

#[derive(Debug)]
pub struct SlotCache {
    cache: DashMap<Pubkey, Arc<CachedAccount>, AHashRandomState>,
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

    pub fn insert(&self, pubkey: &Pubkey, account: AccountSharedData) -> Arc<CachedAccount> {
        let data_len = account.data().len() as u64;
        let item = Arc::new(CachedAccount {
            account,
            pubkey: *pubkey,
        });
        if let Some(old) = self.cache.insert(*pubkey, item.clone()) {
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
        } else {
            self.size.fetch_add(data_len, Ordering::Relaxed);
            self.total_size.fetch_add(data_len, Ordering::Relaxed);
            self.unique_account_writes_size
                .fetch_add(data_len, Ordering::Relaxed);
            self.accounts_count.fetch_add(1, Ordering::Relaxed);
            self.total_accounts_count.fetch_add(1, Ordering::Relaxed);
        }
        item
    }

    pub fn get_cloned(&self, pubkey: &Pubkey) -> Option<Arc<CachedAccount>> {
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
    type Target = DashMap<Pubkey, Arc<CachedAccount>, AHashRandomState>;
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

#[derive(Debug, Default)]
pub struct AccountsCache {
    cache: DashMap<Slot, Arc<SlotCache>, BuildNoHashHasher<Slot>>,
    // Queue of potentially unflushed roots. Random eviction + cache too large
    // could have triggered a flush of this slot already
    maybe_unflushed_roots: RwLock<BTreeSet<Slot>>,
    max_flushed_root: AtomicU64,
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

        slot_cache.insert(pubkey, account)
    }

    pub fn load(&self, slot: Slot, pubkey: &Pubkey) -> Option<Arc<CachedAccount>> {
        self.slot_cache(slot)
            .and_then(|slot_cache| slot_cache.get_cloned(pubkey))
    }

    pub fn remove_slot(&self, slot: Slot) -> Option<Arc<SlotCache>> {
        self.cache.remove(&slot).map(|(_, slot_cache)| slot_cache)
    }

    pub fn slot_cache(&self, slot: Slot) -> Option<Arc<SlotCache>> {
        self.cache.get(&slot).map(|result| result.value().clone())
    }

    pub fn add_root(&self, root: Slot) {
        self.maybe_unflushed_roots.write().unwrap().insert(root);
    }

    pub fn clear_roots(&self, max_root: Option<Slot>) -> BTreeSet<Slot> {
        let mut w_maybe_unflushed_roots = self.maybe_unflushed_roots.write().unwrap();
        if let Some(max_root) = max_root {
            // `greater_than_max_root` contains all slots >= `max_root + 1`, or alternatively,
            // all slots > `max_root`. Meanwhile, `w_maybe_unflushed_roots` is left with all slots
            // <= `max_root`.
            let greater_than_max_root = w_maybe_unflushed_roots.split_off(&(max_root + 1));
            // After the replace, `w_maybe_unflushed_roots` contains slots > `max_root`, and
            // we return all slots <= `max_root`
            std::mem::replace(&mut w_maybe_unflushed_roots, greater_than_max_root)
        } else {
            std::mem::take(&mut *w_maybe_unflushed_roots)
        }
    }

    pub fn contains_any_slots(&self, max_slot_inclusive: Slot) -> bool {
        self.cache.iter().any(|e| e.key() <= &max_slot_inclusive)
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

    pub fn fetch_max_flush_root(&self) -> Slot {
        self.max_flushed_root.load(Ordering::Acquire)
    }

    pub fn set_max_flush_root(&self, root: Slot) {
        self.max_flushed_root.fetch_max(root, Ordering::Release);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

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
}
