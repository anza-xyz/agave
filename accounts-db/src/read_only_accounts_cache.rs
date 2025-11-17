//! ReadOnlyAccountsCache used to store accounts, such as executable accounts,
//! which can be large, loaded many times, and rarely change.
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::{field_qualifiers, qualifiers};
use {
    ahash::random_state::RandomState as AHashRandomState,
    dashmap::{mapref::entry::Entry, DashMap},
    log::*,
    rand::{
        rng,
        seq::{IndexedRandom as _, IteratorRandom},
        Rng,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    std::{
        mem::ManuallyDrop,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc,
        },
        thread,
        time::{Duration, Instant},
    },
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
const CACHE_ENTRY_SIZE: usize =
    size_of::<ReadOnlyAccountCacheEntry>() + size_of::<ReadOnlyCacheKey>();

type ReadOnlyCacheKey = Pubkey;

#[derive(Debug)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(account(pub), slot(pub), last_update_time(pub))
)]
struct ReadOnlyAccountCacheEntry {
    account: AccountSharedData,
    /// 'slot' tracks when the 'account' is stored. This important for
    /// correctness. When 'loading' from the cache by pubkey+slot, we need to
    /// make sure that both pubkey and slot matches in the cache. Otherwise, we
    /// may return the wrong account.
    slot: Slot,
    /// Timestamp when the entry was updated, in ns
    last_update_time: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
pub struct ReadOnlyCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evicts: u64,
    pub load_us: u64,
    pub store_us: u64,
    pub evict_us: u64,
    pub evictor_wakeup_count_all: u64,
    pub evictor_wakeup_count_productive: u64,
}

#[derive(Default, Debug)]
struct AtomicReadOnlyCacheStats {
    hits: AtomicU64,
    misses: AtomicU64,
    evicts: AtomicU64,
    load_us: AtomicU64,
    store_us: AtomicU64,
    evict_us: AtomicU64,
    evictor_wakeup_count_all: AtomicU64,
    evictor_wakeup_count_productive: AtomicU64,
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Debug)]
pub(crate) struct ReadOnlyAccountsCache {
    cache: Arc<DashMap<ReadOnlyCacheKey, ReadOnlyAccountCacheEntry, AHashRandomState>>,
    _max_data_size_lo: usize,
    _max_data_size_hi: usize,
    data_size: Arc<AtomicUsize>,

    // Performance statistics
    stats: Arc<AtomicReadOnlyCacheStats>,
    highest_slot_stored: AtomicU64,

    /// Timer for generating timestamps for entries.
    timer: Instant,

    /// To the evictor goes the spoiled [sic]
    ///
    /// Evict from the cache in the background.
    evictor_thread_handle: ManuallyDrop<thread::JoinHandle<()>>,
    /// Flag to stop the evictor
    evictor_exit_flag: Arc<AtomicBool>,
}

impl ReadOnlyAccountsCache {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new(
        max_data_size_lo: usize,
        max_data_size_hi: usize,
        evict_sample_size: usize,
    ) -> Self {
        assert!(max_data_size_lo <= max_data_size_hi);
        assert!(evict_sample_size > 0);
        let cache = Arc::new(DashMap::with_hasher(AHashRandomState::default()));
        let data_size = Arc::new(AtomicUsize::default());
        let stats = Arc::new(AtomicReadOnlyCacheStats::default());
        let timer = Instant::now();
        let evictor_exit_flag = Arc::new(AtomicBool::new(false));
        let evictor_thread_handle = Self::spawn_evictor(
            evictor_exit_flag.clone(),
            max_data_size_lo,
            max_data_size_hi,
            data_size.clone(),
            evict_sample_size,
            cache.clone(),
            stats.clone(),
        );

        Self {
            highest_slot_stored: AtomicU64::default(),
            _max_data_size_lo: max_data_size_lo,
            _max_data_size_hi: max_data_size_hi,
            cache,
            data_size,
            stats,
            timer,
            evictor_thread_handle: ManuallyDrop::new(evictor_thread_handle),
            evictor_exit_flag,
        }
    }

    /// true if pubkey is in cache at slot
    pub(crate) fn in_cache(&self, pubkey: &Pubkey, slot: Slot) -> bool {
        if let Some(entry) = self.cache.get(pubkey) {
            entry.slot == slot
        } else {
            false
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn load(&self, pubkey: Pubkey, slot: Slot) -> Option<AccountSharedData> {
        let (account, load_us) = measure_us!({
            let mut found = None;
            if let Some(entry) = self.cache.get(&pubkey) {
                if entry.slot == slot {
                    entry
                        .last_update_time
                        .store(self.timestamp(), Ordering::Relaxed);
                    let account = entry.account.clone();
                    drop(entry);
                    self.stats.hits.fetch_add(1, Ordering::Relaxed);
                    found = Some(account);
                }
            }

            if found.is_none() {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
            }
            found
        });
        self.stats.load_us.fetch_add(load_us, Ordering::Relaxed);
        account
    }

    fn account_size(account: &AccountSharedData) -> usize {
        CACHE_ENTRY_SIZE + account.data().len()
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn store(&self, pubkey: Pubkey, slot: Slot, account: AccountSharedData) {
        self.store_with_timestamp(pubkey, slot, account, self.timestamp())
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn store_with_timestamp(
        &self,
        pubkey: Pubkey,
        slot: Slot,
        account: AccountSharedData,
        timestamp: u64,
    ) {
        let measure_store = Measure::start("");
        self.highest_slot_stored.fetch_max(slot, Ordering::Release);
        let account_size = Self::account_size(&account);
        self.data_size.fetch_add(account_size, Ordering::Relaxed);
        match self.cache.entry(pubkey) {
            Entry::Vacant(entry) => {
                entry.insert(ReadOnlyAccountCacheEntry::new(account, slot, timestamp));
            }
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                let account_size = Self::account_size(&entry.account);
                self.data_size.fetch_sub(account_size, Ordering::Relaxed);
                entry.account = account;
                entry.slot = slot;
                entry.last_update_time.store(timestamp, Ordering::Relaxed);
            }
        };
        let store_us = measure_store.end_as_us();
        self.stats.store_us.fetch_add(store_us, Ordering::Relaxed);
    }

    /// true if any pubkeys could have ever been stored into the cache at `slot`
    pub(crate) fn can_slot_be_in_cache(&self, slot: Slot) -> bool {
        self.highest_slot_stored.load(Ordering::Acquire) >= slot
    }

    /// remove entry if it exists.
    /// Assume the entry does not exist for performance.
    pub(crate) fn remove_assume_not_present(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        // get read lock first to see if the entry exists
        self.cache
            .contains_key(pubkey)
            .then(|| self.remove(pubkey))
            .flatten()
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn remove(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        Self::do_remove(pubkey, &self.cache, &self.data_size).map(|entry| entry.account)
    }

    /// Removes `key` from the cache, if present, and returns the account entry.
    fn do_remove(
        key: &ReadOnlyCacheKey,
        cache: &DashMap<ReadOnlyCacheKey, ReadOnlyAccountCacheEntry, AHashRandomState>,
        data_size: &AtomicUsize,
    ) -> Option<ReadOnlyAccountCacheEntry> {
        let (_, entry) = cache.remove(key)?;
        let account_size = Self::account_size(&entry.account);
        data_size.fetch_sub(account_size, Ordering::Relaxed);
        Some(entry)
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub(crate) fn data_size(&self) -> usize {
        self.data_size.load(Ordering::Relaxed)
    }

    pub(crate) fn get_and_reset_stats(&self) -> ReadOnlyCacheStats {
        let hits = self.stats.hits.swap(0, Ordering::Relaxed);
        let misses = self.stats.misses.swap(0, Ordering::Relaxed);
        let evicts = self.stats.evicts.swap(0, Ordering::Relaxed);
        let load_us = self.stats.load_us.swap(0, Ordering::Relaxed);
        let store_us = self.stats.store_us.swap(0, Ordering::Relaxed);
        let evict_us = self.stats.evict_us.swap(0, Ordering::Relaxed);
        let evictor_wakeup_count_all = self
            .stats
            .evictor_wakeup_count_all
            .swap(0, Ordering::Relaxed);
        let evictor_wakeup_count_productive = self
            .stats
            .evictor_wakeup_count_productive
            .swap(0, Ordering::Relaxed);

        ReadOnlyCacheStats {
            hits,
            misses,
            evicts,
            load_us,
            store_us,
            evict_us,
            evictor_wakeup_count_all,
            evictor_wakeup_count_productive,
        }
    }

    /// Spawns the background thread to handle evictions
    fn spawn_evictor(
        exit: Arc<AtomicBool>,
        max_data_size_lo: usize,
        max_data_size_hi: usize,
        data_size: Arc<AtomicUsize>,
        evict_sample_size: usize,
        cache: Arc<DashMap<ReadOnlyCacheKey, ReadOnlyAccountCacheEntry, AHashRandomState>>,
        stats: Arc<AtomicReadOnlyCacheStats>,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("solAcctReadCache".to_string())
            .spawn(move || {
                info!("AccountsReadCacheEvictor has started");
                let mut rng = rng();
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    // We shouldn't need to evict often, so sleep to reduce the frequency.
                    // 100 ms is already four times per slot, which should be plenty.
                    thread::sleep(Duration::from_millis(100));
                    stats
                        .evictor_wakeup_count_all
                        .fetch_add(1, Ordering::Relaxed);

                    if data_size.load(Ordering::Relaxed) <= max_data_size_hi {
                        continue;
                    }
                    stats
                        .evictor_wakeup_count_productive
                        .fetch_add(1, Ordering::Relaxed);

                    #[cfg(not(feature = "dev-context-only-utils"))]
                    let (num_evicts, evict_us) = measure_us!(Self::evict(
                        max_data_size_lo,
                        &data_size,
                        evict_sample_size,
                        &cache,
                        &mut rng,
                    ));
                    #[cfg(feature = "dev-context-only-utils")]
                    let (num_evicts, evict_us) = measure_us!(Self::evict(
                        max_data_size_lo,
                        &data_size,
                        evict_sample_size,
                        &cache,
                        &mut rng,
                        |_, _| {}
                    ));
                    stats.evicts.fetch_add(num_evicts, Ordering::Relaxed);
                    stats.evict_us.fetch_add(evict_us, Ordering::Relaxed);
                }
                info!("AccountsReadCacheEvictor has stopped");
            })
            .expect("spawn accounts read cache evictor thread")
    }

    /// Evicts entries until the cache's size is <= `target_data_size`,
    /// following the sampled LRU eviction method, where a sample of size
    /// `evict_sample_size` is randomly selected from the cache, using the
    /// provided `rng`.
    ///
    /// Returns the number of entries evicted.
    fn evict<R>(
        target_data_size: usize,
        data_size: &AtomicUsize,
        evict_sample_size: usize,
        cache: &DashMap<ReadOnlyCacheKey, ReadOnlyAccountCacheEntry, AHashRandomState>,
        rng: &mut R,
        #[cfg(feature = "dev-context-only-utils")] mut callback: impl FnMut(
            &Pubkey,
            Option<ReadOnlyAccountCacheEntry>,
        ),
    ) -> u64
    where
        R: Rng,
    {
        let mut num_evicts: u64 = 0;
        while data_size.load(Ordering::Relaxed) > target_data_size {
            let mut key_to_evict = None;
            let mut min_update_time = u64::MAX;
            let mut remaining_samples = evict_sample_size;
            // NOTE: This can loop indefinitely if the cache is misconfigured
            // and when we get here there aren't at least `evict_sample_size`
            // elements. We could break the loop on `cache.is_empty()` but
            // calling `is_empty()` and `len()` on a dashmap is very expensive
            // as it requires iterating and locking all the shards. So, avoid
            // paying that cost and assume that when eviction triggers the
            // cache contains enough items.
            while remaining_samples > 0 {
                let shard = cache
                    .shards()
                    .choose(rng)
                    .expect("number of shards should be greater than zero");
                let shard = shard.read();
                for (key, entry) in shard.iter().choose_multiple(rng, remaining_samples) {
                    let last_update_time = entry.get().last_update_time.load(Ordering::Relaxed);
                    if last_update_time < min_update_time {
                        min_update_time = last_update_time;
                        key_to_evict = Some(key.to_owned());
                    }

                    remaining_samples = remaining_samples.saturating_sub(1);
                }
            }

            let key = key_to_evict.expect("eviction sample should not be empty");
            let _entry = Self::do_remove(&key, cache, data_size);
            #[cfg(feature = "dev-context-only-utils")]
            {
                #[allow(clippy::used_underscore_binding)]
                callback(&key, _entry);
            }
            num_evicts = num_evicts.saturating_add(1);
        }
        num_evicts
    }

    /// Return the elapsed time of the cache.
    fn timestamp(&self) -> u64 {
        self.timer.elapsed().as_nanos() as u64
    }

    // Evict entries, but in the foreground
    //
    // Evicting in the background is non-deterministic w.r.t. when the evictor runs,
    // which can make asserting invariants difficult in tests.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn evict_in_foreground<R, C>(
        &self,
        evict_sample_size: usize,
        rng: &mut R,
        callback: C,
    ) -> u64
    where
        R: Rng,
        C: FnMut(&Pubkey, Option<ReadOnlyAccountCacheEntry>),
    {
        #[allow(clippy::used_underscore_binding)]
        let target_data_size = self._max_data_size_lo;
        Self::evict(
            target_data_size,
            &self.data_size,
            evict_sample_size,
            &self.cache,
            rng,
            callback,
        )
    }
}

impl Drop for ReadOnlyAccountsCache {
    fn drop(&mut self) {
        self.evictor_exit_flag.store(true, Ordering::Relaxed);
        // SAFETY: We are dropping, so we will never use `evictor_thread_handle` again.
        let evictor_thread_handle = unsafe { ManuallyDrop::take(&mut self.evictor_thread_handle) };
        evictor_thread_handle
            .join()
            .expect("join accounts read cache evictor thread");
    }
}

impl ReadOnlyAccountCacheEntry {
    fn new(account: AccountSharedData, slot: Slot, timestamp: u64) -> Self {
        Self {
            account,
            slot,
            last_update_time: AtomicU64::new(timestamp),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::{Rng, SeedableRng},
        rand_chacha::ChaChaRng,
        solana_account::Account,
        std::{
            collections::HashMap,
            iter::repeat_with,
            sync::Arc,
            time::{Duration, Instant},
        },
        test_case::test_matrix,
    };

    impl ReadOnlyAccountsCache {
        /// reset the read only accounts cache
        #[cfg(feature = "dev-context-only-utils")]
        pub fn reset_for_tests(&self) {
            self.cache.clear();
            self.data_size.store(0, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_accountsdb_sizeof() {
        // size_of(arc(x)) does not return the size of x
        assert!(std::mem::size_of::<Arc<u64>>() == std::mem::size_of::<Arc<u8>>());
        assert!(std::mem::size_of::<Arc<u64>>() == std::mem::size_of::<Arc<[u8; 32]>>());
    }

    /// Checks the integrity of data stored in the cache after sequence of
    /// loads and stores.
    #[test_matrix([10, 16])]
    fn test_read_only_accounts_cache_random(evict_sample_size: usize) {
        const SEED: [u8; 32] = [0xdb; 32];
        const DATA_SIZE: usize = 19;
        const MAX_CACHE_SIZE: usize = 17 * (CACHE_ENTRY_SIZE + DATA_SIZE);
        let mut rng = ChaChaRng::from_seed(SEED);
        let cache = ReadOnlyAccountsCache::new(
            MAX_CACHE_SIZE,
            usize::MAX, // <-- do not evict in the background
            evict_sample_size,
        );
        let slots: Vec<Slot> = repeat_with(|| rng.random_range(0..1000)).take(5).collect();
        let pubkeys: Vec<Pubkey> = repeat_with(|| {
            let mut arr = [0u8; 32];
            rng.fill(&mut arr[..]);
            Pubkey::new_from_array(arr)
        })
        .take(35)
        .collect();
        let mut hash_map = HashMap::<ReadOnlyCacheKey, (AccountSharedData, Slot, usize)>::new();
        for ix in 0..1000 {
            if rng.random_bool(0.1) {
                let element = cache.cache.iter().choose(&mut rng).unwrap();
                let (pubkey, entry) = element.pair();
                let slot = entry.slot;
                let account = cache.load(*pubkey, slot).unwrap();
                let (other, other_slot, index) = hash_map.get_mut(pubkey).unwrap();
                assert_eq!(account, *other);
                assert_eq!(slot, *other_slot);
                *index = ix;
            } else {
                let mut data = vec![0u8; DATA_SIZE];
                rng.fill(&mut data[..]);
                let account = AccountSharedData::from(Account {
                    lamports: rng.random(),
                    data,
                    executable: rng.random(),
                    rent_epoch: rng.random(),
                    owner: Pubkey::default(),
                });
                let slot = *slots.choose(&mut rng).unwrap();
                let pubkey = *pubkeys.choose(&mut rng).unwrap();
                hash_map.insert(pubkey, (account.clone(), slot, ix));
                cache.store(pubkey, slot, account);
                cache.evict_in_foreground(evict_sample_size, &mut rng, |_, _| {});
            }
        }
        assert_eq!(cache.cache_len(), 17);
        assert_eq!(hash_map.len(), 35);
        // Ensure that all the cache entries hold information consistent with
        // what we accumulated in the local hash map.
        // Note that the opposite assertion (checking that all entries from the
        // local hash map exist in the cache) wouldn't work, because of sampled
        // LRU eviction.
        for entry in cache.cache.iter() {
            let pubkey = entry.key();
            let ReadOnlyAccountCacheEntry { account, slot, .. } = entry.value();

            let (local_account, local_slot, _) = hash_map
                .get(pubkey)
                .expect("account to be present in the map");
            assert_eq!(account, local_account);
            assert_eq!(slot, local_slot);
        }
    }

    #[test_matrix([8, 10, 16])]
    fn test_evict_in_background(evict_sample_size: usize) {
        const ACCOUNT_DATA_SIZE: usize = 200;
        const MAX_ENTRIES: usize = 7;
        const MAX_CACHE_SIZE: usize = MAX_ENTRIES * (CACHE_ENTRY_SIZE + ACCOUNT_DATA_SIZE);
        let cache = ReadOnlyAccountsCache::new(MAX_CACHE_SIZE, MAX_CACHE_SIZE, evict_sample_size);

        for i in 0..MAX_ENTRIES {
            let pubkey = Pubkey::new_unique();
            let account = AccountSharedData::new(i as u64, ACCOUNT_DATA_SIZE, &Pubkey::default());
            cache.store(pubkey, i as Slot, account);
        }
        // we haven't exceeded the max cache size yet, so no evictions should've happened
        assert_eq!(cache.cache_len(), MAX_ENTRIES);
        assert_eq!(cache.data_size(), MAX_CACHE_SIZE);
        assert_eq!(cache.stats.evicts.load(Ordering::Relaxed), 0);

        // store another account to trigger evictions
        let slot = MAX_ENTRIES as Slot;
        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::new(42, ACCOUNT_DATA_SIZE, &Pubkey::default());
        cache.store(pubkey, slot, account);

        // wait for the evictor to run...
        let timer = Instant::now();
        while cache.stats.evicts.load(Ordering::Relaxed) == 0 {
            assert!(
                timer.elapsed() < Duration::from_secs(5),
                "timed out waiting for the evictor to run",
            );
            thread::sleep(Duration::from_millis(1));
        }

        // ...now ensure the cache size is right
        assert_eq!(cache.cache_len(), MAX_ENTRIES);
        assert_eq!(cache.data_size(), MAX_CACHE_SIZE);
    }
}
