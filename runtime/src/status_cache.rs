use {
    ahash::random_state::RandomState as AHashRandomState,
    dashmap::{mapref::entry::Entry, DashMap, DashSet},
    rand::{thread_rng, Rng},
    serde::{
        de::{SeqAccess, Visitor},
        ser::SerializeSeq as _,
        Deserialize, Deserializer, Serialize,
    },
    smallvec::SmallVec,
    solana_accounts_db::ancestors::Ancestors,
    solana_sdk::{
        clock::{Slot, MAX_RECENT_BLOCKHASHES},
        hash::Hash,
    },
    std::{
        fmt,
        hash::BuildHasher,
        mem::MaybeUninit,
        ptr,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
};

// The maximum number of entries to store in the cache. This is the same as the number of recent
// blockhashes because we automatically reject txs that use older blockhashes so we don't need to
// track those explicitly.
pub const MAX_CACHE_ENTRIES: usize = MAX_RECENT_BLOCKHASHES;

// Only store 20 bytes of the tx keys processed to save some memory.
const CACHED_KEY_SIZE: usize = 20;

// The number of shards to use for ::slot_deltas. We're going to store at most MAX_CACHE_ENTRIES
// slots, MAX * 4 gives us a low load factor guaranteeing that collisions are vey rare.
const SLOT_SHARDS: usize = (MAX_CACHE_ENTRIES * 4).next_power_of_two();

// MAX_CACHE_ENTRIES = MAX_RECENT_BLOCKHASHES. We only insert txs with valid blockhashes, so as
// above multiply by 4 to reduce load factor to make collisions unlikely to happen.
const BLOCKHASH_SHARDS: usize = SLOT_SHARDS;

// The number of shards used for the maps that hold the actual tx keys.  Collisions here are
// inevitable when doing high TPS. The tradeoff is between having too few shards, which would lead
// to contention due to collisions, and too many shards, which would lead to more memory usage and
// contention when first creating the maps (creation happens with write lock held).
const KEY_SHARDS: usize = 1024;

// Store forks in a single chunk of memory to avoid another hash lookup. Avoid allocations in the
// case a tx only lands in one (the vast majority) or two forks.
pub type ForkStatus<T> = SmallVec<[(Slot, T); 2]>;

// The type of the key used in the cache.
type KeySlice = [u8; CACHED_KEY_SIZE];

// A map that stores map[tx_key] => [(fork1_slot, tx_result), (fork2_slot, tx_result), ...]
type KeyMap<T> = DashMap<KeySlice, ForkStatus<T>, AHashRandomState>;

// The type used for StatusCache::cache. See the field definition for more details.
type KeyStatusMap<T> = ReadOptimizedDashMap<Hash, (AtomicU64, usize, KeyMap<T>), AHashRandomState>;

// The inner type of the StatusCache::slot_deltas map. Stores map[blockhash] => [(tx_key, tx_result), ...]
type StatusInner<T> = DashMap<Hash, (usize, ConcurrentVec<(KeySlice, T)>), AHashRandomState>;

// Arc wrapper around StatusInner so the accounts-db can clone the result of
// StatusCache::slot_deltas() cheaply and process it while the cache is being updated.
pub type Status<T> = Arc<StatusInner<T>>;

// The type used for StatusCache::slot_deltas. See the field definition for more details.
type SlotDeltaMap<T> = ReadOptimizedDashMap<Slot, StatusInner<T>, AHashRandomState>;

// The statuses added during a slot, can be used to build on top of a status cache or to
// construct a new one. Usually derived from a status cache's `SlotDeltaMap`
pub type SlotDelta<T> = (Slot, bool, Status<T>);

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
pub struct StatusCache<T: Serialize + Clone> {
    // cache[blockhash][tx_key] => [(fork1_slot, tx_result), (fork2_slot, tx_result), ...] used to
    // check if a tx_key was seen on a fork and for rpc to retrieve the tx_result
    cache: KeyStatusMap<T>,
    // slot_deltas[slot][blockhash] => [(tx_key, tx_result), ...] used to serialize for snapshots
    // and to rebuild cache[blockhash][tx_key] from a snapshot
    slot_deltas: SlotDeltaMap<T>,
    // set of rooted slots
    roots: DashSet<Slot, AHashRandomState>,
}

impl<T: Serialize + Clone> Default for StatusCache<T> {
    fn default() -> Self {
        Self {
            cache: ReadOptimizedDashMap::new(DashMap::with_hasher_and_shard_amount(
                AHashRandomState::default(),
                BLOCKHASH_SHARDS,
            )),
            slot_deltas: ReadOptimizedDashMap::new(DashMap::with_hasher_and_shard_amount(
                AHashRandomState::default(),
                SLOT_SHARDS,
            )),
            // 0 is always a root
            roots: DashSet::from_iter([0]),
        }
    }
}

impl<T: Serialize + Clone> StatusCache<T> {
    /// Clear all entries for a slot.
    ///
    /// This is used when a slot is purged from the bank, see
    /// ReplayStage::purge_unconfirmed_duplicate_slot(). When this is called, it's guaranteed that
    /// there are no threads inserting new entries for this slot, so there are no races.
    pub fn clear_slot_entries(&self, slot: Slot) {
        // remove txs seen during this slot
        let Some((_, slot_deltas)) = self.slot_deltas.remove(&slot) else {
            return;
        };

        // loop over all the blockhashes referenced by txs inserted in the slot
        for item in slot_deltas.iter() {
            // one of the blockhashes referenced
            let blockhash = item.key();
            // slot_delta_txs is self.slot_deltas[slot][blockhash], ie the txs that referenced
            // `blockhash` in `slot`
            let (_key_index, slot_delta_txs) = item.value();

            // any self.slot_deltas[slot][blockhash] must also exist as self.cache[blockhash] - the
            // two maps store the same blockhashes just in a different layout.
            let Some(cache_guard) = self.cache.get(blockhash) else {
                panic!("Blockhash must exist if it exists in self.slot_deltas, slot: {slot}")
            };

            // cache_txs is self.blockhash_cache[blockhash]
            let (_, _, cache_txs) = &*cache_guard;

            // loop over the txs in slot_delta[slot][blockhash]
            for (_, (key_slice, _)) in slot_delta_txs {
                // find the corresponding tx in self.cache[blockhash]
                let Entry::Occupied(mut cache_tx_entry) = cache_txs.entry(*key_slice) else {
                    panic!("Map for key must exist if key exists in self.slot_deltas, slot: {slot}")
                };
                // remove the slot from the list of forks this tx was inserted in
                let forks = cache_tx_entry.get_mut();
                forks.retain(|(fork_slot, _key)| *fork_slot != slot);
                if forks.is_empty() {
                    // if all the slots have been cleared or purged, we don't need to track this tx
                    // anymore
                    cache_tx_entry.remove();
                }
            }

            // if this blockhash has no more txs, remove it from the cache
            if cache_txs.is_empty() {
                drop(cache_guard);
                self.cache.remove(blockhash);
            }
        }
    }

    /// Check if the key is in any of the forks in the ancestors set and
    /// with a certain blockhash.
    pub fn get_status<K: AsRef<[u8]>>(
        &self,
        key: K,
        blockhash: &Hash,
        ancestors: &Ancestors,
    ) -> Option<(Slot, T)> {
        let (_, key_index, txs) = &*self.cache.get(blockhash)?;
        self.do_get_status(txs, *key_index, &key, ancestors)
    }

    fn do_get_status<K: AsRef<[u8]>>(
        &self,
        txs: &KeyMap<T>,
        key_index: usize,
        key: &K,
        ancestors: &Ancestors,
    ) -> Option<(u64, T)> {
        let max_key_index = key.as_ref().len().saturating_sub(CACHED_KEY_SIZE + 1);
        let index = key_index.min(max_key_index);
        let key_slice: &[u8; CACHED_KEY_SIZE] =
            arrayref::array_ref![key.as_ref(), index, CACHED_KEY_SIZE];

        txs.get(key_slice).and_then(|forks| {
            forks
                .iter()
                .find(|(slot, _)| ancestors.contains_key(slot) || self.roots.contains(slot))
                .cloned()
        })
    }

    /// Search for a key with any blockhash.
    ///
    /// Prefer get_status for performance reasons, it doesn't need to search all blockhashes.
    pub fn get_status_any_blockhash<K: AsRef<[u8]>>(
        &self,
        key: K,
        ancestors: &Ancestors,
    ) -> Option<(Slot, T)> {
        for item in self.cache.iter() {
            let (_, key_index, txs) = &**item.value();
            let status = self.do_get_status(txs, *key_index, &key, ancestors);
            if status.is_some() {
                return status;
            }
        }
        None
    }

    /// Add a known root fork.
    ///
    /// Roots are always valid ancestors. After MAX_CACHE_ENTRIES, roots are removed, and any old
    /// keys are cleared.
    pub fn add_root(&self, fork: Slot) {
        self.roots.insert(fork);
        self.purge_roots();
    }

    /// Get all the roots.
    pub fn roots(&self) -> impl Iterator<Item = Slot> + '_ {
        self.roots.iter().map(|x| *x)
    }

    /// Insert a new key using the given blockhash at the given slot.
    pub fn insert<K: AsRef<[u8]>>(&self, blockhash: &Hash, key: K, slot: Slot, res: T) {
        let max_key_index = key.as_ref().len().saturating_sub(CACHED_KEY_SIZE + 1);
        let mut key_slice = MaybeUninit::<[u8; CACHED_KEY_SIZE]>::uninit();

        // Get the cache entry for this blockhash.
        let key_index = {
            let (max_slot, key_index, txs) = &*self.cache.get_or_insert_with(blockhash, || {
                let key_index = thread_rng().gen_range(0..max_key_index + 1);
                (
                    AtomicU64::new(slot),
                    key_index,
                    DashMap::with_hasher_and_shard_amount(AHashRandomState::default(), KEY_SHARDS),
                )
            });

            // Update the max slot observed to contain txs using this blockhash.
            max_slot.fetch_max(slot, Ordering::Relaxed);

            // Grab the key slice.
            let key_index = (*key_index).min(max_key_index);
            unsafe {
                ptr::copy_nonoverlapping(
                    key.as_ref()[key_index..key_index + CACHED_KEY_SIZE].as_ptr(),
                    key_slice.as_mut_ptr() as *mut u8,
                    CACHED_KEY_SIZE,
                )
            }

            // Insert the slot and tx result into the cache entry associated with
            // this blockhash and keyslice.
            let mut forks = txs.entry(unsafe { key_slice.assume_init() }).or_default();
            forks.push((slot, res.clone()));

            key_index
        };

        self.add_to_slot_delta(
            blockhash,
            slot,
            key_index,
            unsafe { key_slice.assume_init() },
            res,
        );
    }

    fn purge_roots(&self) {
        if self.roots.len() > MAX_CACHE_ENTRIES {
            if let Some(min) = self.roots().min() {
                self.roots.remove(&min);
                self.cache.retain(|_key, value| {
                    let (max_slot, _, _) = &**value;
                    max_slot.load(Ordering::Relaxed) > min
                });
                self.slot_deltas.retain(|slot, _| *slot > min);
            }
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn clear(&self) {
        self.cache.clear();
        self.slot_deltas.clear();
    }

    /// Get the statuses for all the root slots.
    ///
    /// This is never called concurrently with add_root(), and for a slot to be a root there must be
    /// no new entries for that slot, so there are no races.
    ///
    /// See ReplayStage::handle_new_root() => BankForks::set_root() =>
    /// BankForks::do_set_root_return_metrics() => root_slot_deltas()
    pub fn root_slot_deltas(&self) -> Vec<SlotDelta<T>> {
        self.roots()
            .map(|root| {
                (
                    root,
                    true, // <-- is_root
                    self.slot_deltas.get(&root).unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Populate the cache with the slot deltas from a snapshot.
    ///
    /// Really badly named method. See load_bank_forks() => ... =>
    /// rebuild_bank_from_snapshot() => [load slot deltas from snapshot] => append()
    pub fn append(&self, slot_deltas: &[SlotDelta<T>]) {
        for (slot, is_root, statuses) in slot_deltas {
            statuses.iter().for_each(|item| {
                let tx_hash = item.key();
                let (key_index, statuses) = item.value();
                for (_, (key_slice, res)) in statuses.iter() {
                    self.insert_with_slice(tx_hash, *slot, *key_index, *key_slice, res.clone())
                }
            });
            if *is_root {
                self.add_root(*slot);
            }
        }
    }

    fn insert_with_slice(
        &self,
        blockhash: &Hash,
        slot: Slot,
        key_index: usize,
        key_slice: [u8; CACHED_KEY_SIZE],
        res: T,
    ) {
        {
            let (max_slot, _, hash_map) = &*self.cache.get_or_insert_with(blockhash, || {
                (
                    AtomicU64::new(slot),
                    key_index,
                    DashMap::with_hasher_and_shard_amount(AHashRandomState::default(), KEY_SHARDS),
                )
            });
            max_slot.fetch_max(slot, Ordering::Relaxed);

            let mut forks = hash_map.entry(key_slice).or_default();
            forks.push((slot, res.clone()));
        }

        self.add_to_slot_delta(blockhash, slot, key_index, key_slice, res);
    }

    // Add this key slice to the list of key slices for this slot and blockhash combo.
    fn add_to_slot_delta(
        &self,
        blockhash: &Hash,
        slot: Slot,
        key_index: usize,
        key_slice: [u8; CACHED_KEY_SIZE],
        res: T,
    ) {
        let fork_entry = self.slot_deltas.get_or_insert_with(&slot, || {
            DashMap::with_hasher_and_shard_amount(AHashRandomState::default(), KEY_SHARDS)
        });

        // In the vast majority of the cases, there will already be an entry for this blockhash, so
        // do a get() first so that we can avoid taking a write lock on the corresponding shard.
        if let Some(fork_entry) = fork_entry.get(blockhash) {
            let (_key_index, txs) = &*fork_entry;
            // txs is a ConcurrentVec so we can push without any locking
            txs.push((key_slice, res));
        } else {
            // Only take the write lock if this is the first time we've seen
            // this blockhash in this slot.
            let (_key_index, txs) = &mut *fork_entry
                .entry(*blockhash)
                .or_insert_with(|| (key_index, ConcurrentVec::new()));
            txs.push((key_slice, res));
        };
    }
}

// Wrapper around boxcar::Vec that implements Serialize and Deserialize.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
pub struct ConcurrentVec<T> {
    vec: boxcar::Vec<T>,
}

impl<T> ConcurrentVec<T> {
    fn new() -> Self {
        Self {
            vec: boxcar::Vec::new(),
        }
    }

    fn push(&self, item: T) {
        self.vec.push(item);
    }

    fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.vec.iter()
    }
}

impl<T> IntoIterator for ConcurrentVec<T> {
    type Item = T;
    type IntoIter = boxcar::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.vec.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a ConcurrentVec<T> {
    type Item = (usize, &'a T);
    type IntoIter = boxcar::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.vec.iter()
    }
}

impl<T: Serialize> Serialize for ConcurrentVec<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.vec.count()))?;
        for (_, element) in &self.vec {
            seq.serialize_element(element)?;
        }
        seq.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ConcurrentVec<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ConcurrentVecVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ConcurrentVecVisitor<T> {
            type Value = ConcurrentVec<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a sequence")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let vec = boxcar::Vec::with_capacity(seq.size_hint().unwrap_or(0));

                while let Some(value) = seq.next_element()? {
                    vec.push(value);
                }

                Ok(ConcurrentVec { vec })
            }
        }

        deserializer.deserialize_seq(ConcurrentVecVisitor(std::marker::PhantomData))
    }
}

// Wrapper around dashmap that stores (K, Arc<V>) to minimize shard contention.
#[derive(Debug)]
pub struct ReadOptimizedDashMap<K, V, S>
where
    K: Clone + Eq + std::hash::Hash,
    S: Clone + BuildHasher,
{
    inner: DashMap<K, Arc<V>, S>,
}

impl<K, V, S> ReadOptimizedDashMap<K, V, S>
where
    K: Clone + Eq + std::hash::Hash,
    S: Clone + BuildHasher,
{
    fn new(inner: DashMap<K, Arc<V>, S>) -> Self {
        Self { inner }
    }

    // Alternative to entry(k).or_insert_with(default) that returns an Arc<V> instead of returning a
    // guard that holds the underlying shard's write lock.
    fn get_or_insert_with(&self, k: &K, default: impl FnOnce() -> V) -> Arc<V> {
        match self.inner.get(k) {
            Some(v) => Arc::clone(&*v),
            None => Arc::clone(
                self.inner
                    .entry(k.clone())
                    .or_insert_with(|| Arc::new(default()))
                    .value(),
            ),
        }
    }

    fn get(&self, k: &K) -> Option<Arc<V>> {
        self.inner.get(k).map(|v| Arc::clone(&v))
    }

    fn iter(&self) -> impl Iterator<Item = dashmap::mapref::multiple::RefMulti<'_, K, Arc<V>, S>> {
        self.inner.iter()
    }

    fn remove(&self, k: &K) -> Option<(K, Arc<V>)> {
        self.inner.remove(k)
    }

    fn retain(&self, f: impl FnMut(&K, &mut Arc<V>) -> bool) {
        self.inner.retain(f)
    }

    #[cfg(feature = "dev-context-only-utils")]
    fn clear(&self) {
        self.inner.clear();
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{hash::hash, signature::Signature},
    };

    type BankStatusCache = StatusCache<()>;

    impl<T: Serialize + Clone> StatusCache<T> {
        fn from_slot_deltas(slot_deltas: &[SlotDelta<T>]) -> Self {
            let cache = Self::default();
            cache.append(slot_deltas);
            cache
        }
    }

    impl<T: Serialize + Clone + PartialEq> PartialEq for StatusCache<T> {
        fn eq(&self, other: &Self) -> bool {
            use std::collections::HashSet;

            let roots = self.roots.iter().map(|x| *x).collect::<HashSet<_>>();
            let other_roots = other.roots.iter().map(|x| *x).collect::<HashSet<_>>();
            roots == other_roots
                && self.cache.iter().all(|item| {
                    let (hash, value) = item.pair();
                    let (max_slot, key_index, hash_map) = &**value;
                    if let Some(item) = other.cache.get(hash) {
                        let (other_max_slot, other_key_index, other_hash_map) = &*item;
                        if max_slot.load(Ordering::Relaxed)
                            == other_max_slot.load(Ordering::Relaxed)
                            && key_index == other_key_index
                        {
                            return hash_map.iter().all(|item| {
                                let slice = item.key();
                                let fork_map = item.value();
                                if let Some(other_fork_map) = other_hash_map.get(slice) {
                                    // all this work just to compare the highest forks in the fork map
                                    // per entry
                                    return fork_map.last() == other_fork_map.last();
                                }
                                false
                            });
                        }
                    }
                    false
                })
        }
    }

    #[test]
    fn test_empty_has_no_sigs() {
        let sig = Signature::default();
        let blockhash = hash(Hash::default().as_ref());
        let status_cache = BankStatusCache::default();
        assert_eq!(
            status_cache.get_status(sig, &blockhash, &Ancestors::default()),
            None
        );
        assert_eq!(
            status_cache.get_status_any_blockhash(sig, &Ancestors::default()),
            None
        );
    }

    #[test]
    fn test_find_sig_with_ancestor_fork() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = vec![(0, 1)].into_iter().collect();
        status_cache.insert(&blockhash, sig, 0, ());
        assert_eq!(
            status_cache.get_status(sig, &blockhash, &ancestors),
            Some((0, ()))
        );
        assert_eq!(
            status_cache.get_status_any_blockhash(sig, &ancestors),
            Some((0, ()))
        );
    }

    #[test]
    fn test_find_sig_without_ancestor_fork() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = Ancestors::default();
        status_cache.insert(&blockhash, sig, 1, ());
        assert_eq!(status_cache.get_status(sig, &blockhash, &ancestors), None);
        assert_eq!(status_cache.get_status_any_blockhash(sig, &ancestors), None);
    }

    #[test]
    fn test_find_sig_with_root_ancestor_fork() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = Ancestors::default();
        status_cache.insert(&blockhash, sig, 0, ());
        status_cache.add_root(0);
        assert_eq!(
            status_cache.get_status(sig, &blockhash, &ancestors),
            Some((0, ()))
        );
    }

    #[test]
    fn test_insert_picks_latest_blockhash_fork() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = vec![(0, 0)].into_iter().collect();
        status_cache.insert(&blockhash, sig, 0, ());
        status_cache.insert(&blockhash, sig, 1, ());
        for i in 0..(MAX_CACHE_ENTRIES + 1) {
            status_cache.add_root(i as u64);
        }
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors)
            .is_some());
    }

    #[test]
    fn test_root_expires() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = Ancestors::default();
        status_cache.insert(&blockhash, sig, 0, ());
        for i in 0..(MAX_CACHE_ENTRIES + 1) {
            status_cache.add_root(i as u64);
        }
        assert_eq!(status_cache.get_status(sig, &blockhash, &ancestors), None);
    }

    #[test]
    fn test_clear_signatures_sigs_are_gone() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = Ancestors::default();
        status_cache.insert(&blockhash, sig, 0, ());
        status_cache.add_root(0);
        status_cache.clear();
        assert_eq!(status_cache.get_status(sig, &blockhash, &ancestors), None);
    }

    #[test]
    fn test_clear_signatures_insert_works() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = Ancestors::default();
        status_cache.add_root(0);
        status_cache.clear();
        status_cache.insert(&blockhash, sig, 0, ());
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors)
            .is_some());
    }

    #[test]
    fn test_signatures_slice() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        status_cache.clear();
        status_cache.insert(&blockhash, sig, 0, ());
        let (_, index, sig_map) = &*status_cache.cache.get(&blockhash).unwrap();
        let sig_slice: &[u8; CACHED_KEY_SIZE] =
            arrayref::array_ref![sig.as_ref(), *index, CACHED_KEY_SIZE];
        assert!(sig_map.get(sig_slice).is_some());
    }

    #[test]
    fn test_slot_deltas() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        status_cache.clear();
        status_cache.insert(&blockhash, sig, 0, ());
        assert!(status_cache.roots().collect::<Vec<_>>().contains(&0));
        let slot_deltas = status_cache.root_slot_deltas();
        let cache = StatusCache::from_slot_deltas(&slot_deltas);
        assert_eq!(cache, status_cache);
        let slot_deltas = cache.root_slot_deltas();
        let cache = StatusCache::from_slot_deltas(&slot_deltas);
        assert_eq!(cache, status_cache);
    }

    #[test]
    fn test_roots_deltas() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let blockhash2 = hash(blockhash.as_ref());
        status_cache.insert(&blockhash, sig, 0, ());
        status_cache.insert(&blockhash, sig, 1, ());
        status_cache.insert(&blockhash2, sig, 1, ());
        for i in 0..(MAX_CACHE_ENTRIES + 1) {
            status_cache.add_root(i as u64);
        }
        assert!(status_cache.slot_deltas.get(&1).is_some());
        let slot_deltas = status_cache.root_slot_deltas();
        let cache = StatusCache::from_slot_deltas(&slot_deltas);
        assert_eq!(cache, status_cache);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_age_sanity() {
        assert!(MAX_CACHE_ENTRIES <= MAX_RECENT_BLOCKHASHES);
    }

    #[test]
    fn test_clear_slot_signatures() {
        let sig = Signature::default();
        let status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let blockhash2 = hash(blockhash.as_ref());
        status_cache.insert(&blockhash, sig, 0, ());
        status_cache.insert(&blockhash, sig, 1, ());
        status_cache.insert(&blockhash2, sig, 1, ());

        let mut ancestors0 = Ancestors::default();
        ancestors0.insert(0, 0);
        let mut ancestors1 = Ancestors::default();
        ancestors1.insert(1, 0);

        // Clear slot 0 related data
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors0)
            .is_some());
        status_cache.clear_slot_entries(0);
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors0)
            .is_none());
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors1)
            .is_some());
        assert!(status_cache
            .get_status(sig, &blockhash2, &ancestors1)
            .is_some());

        // Check that the slot delta for slot 0 is gone, but slot 1 still
        // exists
        assert!(status_cache.slot_deltas.get(&0).is_none());
        assert!(status_cache.slot_deltas.get(&1).is_some());

        // Clear slot 1 related data
        status_cache.clear_slot_entries(1);
        assert!(status_cache.slot_deltas.get(&0).is_none());
        assert!(status_cache.slot_deltas.get(&1).is_none());
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors1)
            .is_none());
        assert!(status_cache
            .get_status(sig, &blockhash2, &ancestors1)
            .is_none());
    }

    // Status cache uses a random key offset for each blockhash. Ensure that shorter
    // keys can still be used if the offset if greater than the key length.
    #[test]
    fn test_different_sized_keys() {
        let status_cache = BankStatusCache::default();
        let ancestors = vec![(0, 0)].into_iter().collect();
        let blockhash = Hash::default();
        for _ in 0..100 {
            let blockhash = hash(blockhash.as_ref());
            let sig_key = Signature::default();
            let hash_key = Hash::new_unique();
            status_cache.insert(&blockhash, sig_key, 0, ());
            status_cache.insert(&blockhash, hash_key, 0, ());
            assert!(status_cache
                .get_status(sig_key, &blockhash, &ancestors)
                .is_some());
            assert!(status_cache
                .get_status(hash_key, &blockhash, &ancestors)
                .is_some());
        }
    }
}
