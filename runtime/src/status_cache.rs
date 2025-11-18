#[cfg(feature = "shuttle-test")]
use shuttle::sync::{Arc, Mutex};
use {
    ahash::{HashMap, HashMapExt as _},
    log::*,
    serde::Serialize,
    solana_accounts_db::ancestors::Ancestors,
    solana_clock::{Slot, MAX_RECENT_BLOCKHASHES},
    solana_hash::Hash,
    std::collections::{hash_map::Entry, HashSet},
};
#[cfg(not(feature = "shuttle-test"))]
use {
    rand::{rng, Rng},
    std::sync::{Arc, Mutex},
};

// The maximum number of entries to store in the cache. This is the same as the number of recent
// blockhashes because we automatically reject txs that use older blockhashes so we don't need to
// track those explicitly.
pub const MAX_CACHE_ENTRIES: usize = MAX_RECENT_BLOCKHASHES;

// Only store 20 bytes of the tx keys processed to save some memory.
const CACHED_KEY_SIZE: usize = 20;

// Store forks in a single chunk of memory to avoid another hash lookup.
pub type ForkStatus<T> = Vec<(Slot, T)>;

// The type of the key used in the cache.
pub(crate) type KeySlice = [u8; CACHED_KEY_SIZE];

type KeyMap<T> = HashMap<KeySlice, ForkStatus<T>>;

// Map of Hash and status
pub type Status<T> = Arc<Mutex<HashMap<Hash, (usize, Vec<(KeySlice, T)>)>>>;

// A Map of hash + the highest fork it's been observed on along with
// the key offset and a Map of the key slice + Fork status for that key
type KeyStatusMap<T> = HashMap<Hash, (Slot, usize, KeyMap<T>)>;

// The type used for StatusCache::slot_deltas. See the field definition for more details.
type SlotDeltaMap<T> = HashMap<Slot, Status<T>>;

// The statuses added during a slot, can be used to build on top of a status cache or to
// construct a new one. Usually derived from a status cache's `SlotDeltaMap`
pub type SlotDelta<T> = (Slot, bool, Status<T>);

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Debug)]
pub struct StatusCache<T: Serialize + Clone> {
    // cache[blockhash][tx_key] => [(fork1_slot, tx_result), (fork2_slot, tx_result), ...] used to
    // check if a tx_key was seen on a fork and for rpc to retrieve the tx_result
    cache: KeyStatusMap<T>,
    roots: HashSet<Slot>,
    // slot_deltas[slot][blockhash] => [(tx_key, tx_result), ...] used to serialize for snapshots
    // and to rebuild cache[blockhash][tx_key] from a snapshot
    slot_deltas: SlotDeltaMap<T>,
}

impl<T: Serialize + Clone> Default for StatusCache<T> {
    fn default() -> Self {
        Self {
            cache: HashMap::default(),
            // 0 is always a root
            roots: HashSet::from([0]),
            slot_deltas: HashMap::default(),
        }
    }
}

impl<T: Serialize + Clone> StatusCache<T> {
    /// Clear all entries for a slot.
    ///
    /// This is used when a slot is purged from the cache, see
    /// ReplayStage::purge_unconfirmed_duplicate_slot().
    ///
    /// When this is called, it's guaranteed that there are no threads inserting new entries for
    /// this slot. root_slot_deltas() also never accesses slots that are being cleared because roots
    /// are never purged.
    pub fn clear_slot_entries(&mut self, slot: Slot) {
        let slot_deltas = self.slot_deltas.remove(&slot);
        if let Some(slot_deltas) = slot_deltas {
            let slot_deltas = slot_deltas.lock().unwrap();
            for (blockhash, (_, key_list)) in slot_deltas.iter() {
                // Any blockhash that exists in self.slot_deltas must also exist
                // in self.cache, because in self.purge_roots(), when an entry
                // (b, (max_slot, _, _)) is removed from self.cache, this implies
                // all entries in self.slot_deltas < max_slot are also removed
                if let Entry::Occupied(mut o_blockhash_entries) = self.cache.entry(*blockhash) {
                    let (_, _, all_hash_maps) = o_blockhash_entries.get_mut();

                    for (key_slice, _) in key_list {
                        if let Entry::Occupied(mut o_key_list) = all_hash_maps.entry(*key_slice) {
                            let key_list = o_key_list.get_mut();
                            key_list.retain(|(updated_slot, _)| *updated_slot != slot);
                            if key_list.is_empty() {
                                o_key_list.remove_entry();
                            }
                        } else {
                            panic!(
                                "Map for key must exist if key exists in self.slot_deltas, slot: \
                                 {slot}"
                            )
                        }
                    }

                    if all_hash_maps.is_empty() {
                        o_blockhash_entries.remove_entry();
                    }
                } else {
                    panic!("Blockhash must exist if it exists in self.slot_deltas, slot: {slot}")
                }
            }
        }
    }

    /// Check if the key is in any of the forks in the ancestors set and
    /// with a certain blockhash.
    pub fn get_status<K: AsRef<[u8]>>(
        &self,
        key: K,
        transaction_blockhash: &Hash,
        ancestors: &Ancestors,
    ) -> Option<(Slot, T)> {
        let map = self.cache.get(transaction_blockhash)?;
        let (_, index, keymap) = map;
        let max_key_index = key.as_ref().len().saturating_sub(CACHED_KEY_SIZE + 1);
        let index = (*index).min(max_key_index);
        let key_slice: &[u8; CACHED_KEY_SIZE] =
            arrayref::array_ref![key.as_ref(), index, CACHED_KEY_SIZE];
        if let Some(stored_forks) = keymap.get(key_slice) {
            let res = stored_forks
                .iter()
                .find(|(f, _)| ancestors.contains_key(f) || self.roots.contains(f))
                .cloned();
            if res.is_some() {
                return res;
            }
        }
        None
    }

    /// Search for a key with any blockhash.
    ///
    /// Prefer get_status for performance reasons, it doesn't need to search all blockhashes.
    pub fn get_status_any_blockhash<K: AsRef<[u8]>>(
        &self,
        key: K,
        ancestors: &Ancestors,
    ) -> Option<(Slot, T)> {
        let keys: Vec<_> = self.cache.keys().copied().collect();

        for blockhash in keys.iter() {
            trace!("get_status_any_blockhash: trying {blockhash}");
            let status = self.get_status(&key, blockhash, ancestors);
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
    pub fn add_root(&mut self, fork: Slot) {
        self.roots.insert(fork);
        self.purge_roots();
    }

    pub fn roots(&self) -> &HashSet<Slot> {
        &self.roots
    }

    /// Insert a new key using the given blockhash at the given slot.
    pub fn insert<K: AsRef<[u8]>>(
        &mut self,
        transaction_blockhash: &Hash,
        key: K,
        slot: Slot,
        res: T,
    ) {
        let max_key_index = key.as_ref().len().saturating_sub(CACHED_KEY_SIZE + 1);

        // Get the cache entry for this blockhash.
        let (max_slot, key_index, hash_map) =
            self.cache.entry(*transaction_blockhash).or_insert_with(|| {
                // DFS tests need deterministic behavior
                #[cfg(feature = "shuttle-test")]
                let key_index = 0;
                #[cfg(not(feature = "shuttle-test"))]
                let key_index = rng().random_range(0..max_key_index + 1);
                (slot, key_index, HashMap::new())
            });

        // Update the max slot observed to contain txs using this blockhash.
        *max_slot = std::cmp::max(slot, *max_slot);

        // Grab the key slice.
        let key_index = (*key_index).min(max_key_index);
        let mut key_slice = [0u8; CACHED_KEY_SIZE];
        key_slice.clone_from_slice(&key.as_ref()[key_index..key_index + CACHED_KEY_SIZE]);

        // Insert the slot and tx result into the cache entry associated with
        // this blockhash and keyslice.
        let forks = hash_map.entry(key_slice).or_default();
        forks.push((slot, res.clone()));

        self.add_to_slot_delta(transaction_blockhash, slot, key_index, key_slice, res);
    }

    pub fn purge_roots(&mut self) {
        if self.roots.len() > MAX_CACHE_ENTRIES {
            if let Some(min) = self.roots.iter().min().cloned() {
                self.roots.remove(&min);
                self.cache.retain(|_, (fork, _, _)| *fork > min);
                self.slot_deltas.retain(|slot, _| *slot > min);
            }
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn clear(&mut self) {
        for v in self.cache.values_mut() {
            v.2 = HashMap::new();
        }

        self.slot_deltas
            .iter_mut()
            .for_each(|(_, status)| status.lock().unwrap().clear());
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
            .iter()
            .map(|root| {
                (
                    *root,
                    true, // <-- is_root
                    self.slot_deltas.get(root).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Populate the cache with the slot deltas from a snapshot.
    ///
    /// Really badly named method. See load_bank_forks() => ... =>
    /// rebuild_bank_from_snapshot() => [load slot deltas from snapshot] => append()
    pub fn append(&mut self, slot_deltas: &[SlotDelta<T>]) {
        for (slot, is_root, statuses) in slot_deltas {
            statuses
                .lock()
                .unwrap()
                .iter()
                .for_each(|(tx_hash, (key_index, statuses))| {
                    for (key_slice, res) in statuses.iter() {
                        self.insert_with_slice(tx_hash, *slot, *key_index, *key_slice, res.clone())
                    }
                });
            if *is_root {
                self.add_root(*slot);
            }
        }
    }

    fn insert_with_slice(
        &mut self,
        transaction_blockhash: &Hash,
        slot: Slot,
        key_index: usize,
        key_slice: [u8; CACHED_KEY_SIZE],
        res: T,
    ) {
        let hash_map =
            self.cache
                .entry(*transaction_blockhash)
                .or_insert((slot, key_index, HashMap::new()));
        hash_map.0 = std::cmp::max(slot, hash_map.0);

        let forks = hash_map.2.entry(key_slice).or_default();
        forks.push((slot, res.clone()));

        self.add_to_slot_delta(transaction_blockhash, slot, key_index, key_slice, res);
    }

    // Add this key slice to the list of key slices for this slot and blockhash combo.
    fn add_to_slot_delta(
        &mut self,
        transaction_blockhash: &Hash,
        slot: Slot,
        key_index: usize,
        key_slice: [u8; CACHED_KEY_SIZE],
        res: T,
    ) {
        let mut fork_entry = self.slot_deltas.entry(slot).or_default().lock().unwrap();
        let (_key_index, hash_entry) = fork_entry
            .entry(*transaction_blockhash)
            .or_insert((key_index, vec![]));
        hash_entry.push((key_slice, res))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_sha256_hasher::hash, solana_signature::Signature};

    type BankStatusCache = StatusCache<()>;

    impl<T: Serialize + Clone> StatusCache<T> {
        fn from_slot_deltas(slot_deltas: &[SlotDelta<T>]) -> Self {
            let mut cache = Self::default();
            cache.append(slot_deltas);
            cache
        }
    }

    impl<T: Serialize + Clone + PartialEq> PartialEq for StatusCache<T> {
        fn eq(&self, other: &Self) -> bool {
            self.roots == other.roots
                && self
                    .cache
                    .iter()
                    .all(|(hash, (slot, key_index, hash_map))| {
                        if let Some((other_slot, other_key_index, other_hash_map)) =
                            other.cache.get(hash)
                        {
                            if slot == other_slot && key_index == other_key_index {
                                return hash_map.iter().all(|(slice, fork_map)| {
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
        let mut status_cache = BankStatusCache::default();
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
        let mut status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let ancestors = Ancestors::default();
        status_cache.insert(&blockhash, sig, 1, ());
        assert_eq!(status_cache.get_status(sig, &blockhash, &ancestors), None);
        assert_eq!(status_cache.get_status_any_blockhash(sig, &ancestors), None);
    }

    #[test]
    fn test_find_sig_with_root_ancestor_fork() {
        let sig = Signature::default();
        let mut status_cache = BankStatusCache::default();
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
        let mut status_cache = BankStatusCache::default();
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
        let mut status_cache = BankStatusCache::default();
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
        let mut status_cache = BankStatusCache::default();
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
        let mut status_cache = BankStatusCache::default();
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
        let mut status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        status_cache.clear();
        status_cache.insert(&blockhash, sig, 0, ());
        let (_, index, sig_map) = status_cache.cache.get(&blockhash).unwrap();
        let sig_slice: &[u8; CACHED_KEY_SIZE] =
            arrayref::array_ref![sig.as_ref(), *index, CACHED_KEY_SIZE];
        assert!(sig_map.get(sig_slice).is_some());
    }

    #[test]
    fn test_slot_deltas() {
        let sig = Signature::default();
        let mut status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        status_cache.clear();
        status_cache.insert(&blockhash, sig, 0, ());
        assert!(status_cache.roots().contains(&0));
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
        let mut status_cache = BankStatusCache::default();
        let blockhash = hash(Hash::default().as_ref());
        let blockhash2 = hash(blockhash.as_ref());
        status_cache.insert(&blockhash, sig, 0, ());
        status_cache.insert(&blockhash, sig, 1, ());
        status_cache.insert(&blockhash2, sig, 1, ());
        for i in 0..(MAX_CACHE_ENTRIES + 1) {
            status_cache.add_root(i as u64);
        }
        assert_eq!(status_cache.slot_deltas.len(), 1);
        assert!(status_cache.slot_deltas.contains_key(&1));
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
        let mut status_cache = BankStatusCache::default();
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
        assert!(!status_cache.slot_deltas.contains_key(&0));
        assert!(status_cache.slot_deltas.contains_key(&1));

        // Clear slot 1 related data
        status_cache.clear_slot_entries(1);
        assert!(status_cache.slot_deltas.is_empty());
        assert!(status_cache
            .get_status(sig, &blockhash, &ancestors1)
            .is_none());
        assert!(status_cache
            .get_status(sig, &blockhash2, &ancestors1)
            .is_none());
        assert!(status_cache.cache.is_empty());
    }

    // Status cache uses a random key offset for each blockhash. Ensure that shorter
    // keys can still be used if the offset if greater than the key length.
    #[test]
    fn test_different_sized_keys() {
        let mut status_cache = BankStatusCache::default();
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

#[cfg(all(test, feature = "shuttle-test"))]
mod shuttle_tests {
    use {super::*, shuttle::sync::RwLock};

    type BankStatusCache = RwLock<StatusCache<()>>;

    const CLEAR_DFS_ITERATIONS: Option<usize> = None;
    const CLEAR_RANDOM_ITERATIONS: usize = 20000;
    const PURGE_DFS_ITERATIONS: Option<usize> = None;
    const PURGE_RANDOM_ITERATIONS: usize = 8000;
    const INSERT_DFS_ITERATIONS: Option<usize> = Some(20000);
    const INSERT_RANDOM_ITERATIONS: usize = 20000;

    fn do_test_shuttle_clear_slots_blockhash_overlap() {
        let status_cache = Arc::new(BankStatusCache::default());

        let blockhash1 = Hash::new_from_array([1; 32]);

        let key1 = Hash::new_from_array([3; 32]);
        let key2 = Hash::new_from_array([4; 32]);

        status_cache
            .write()
            .unwrap()
            .insert(&blockhash1, key1, 1, ());
        let th_clear = shuttle::thread::spawn({
            let status_cache = status_cache.clone();
            move || {
                status_cache.write().unwrap().clear_slot_entries(1);
            }
        });

        let th_insert = shuttle::thread::spawn({
            let status_cache = status_cache.clone();
            move || {
                // insert an entry for slot 1 so clear_slot_entries will remove it
                status_cache
                    .write()
                    .unwrap()
                    .insert(&blockhash1, key2, 2, ());
            }
        });

        th_clear.join().unwrap();
        th_insert.join().unwrap();

        let mut ancestors2 = Ancestors::default();
        ancestors2.insert(2, 0);

        assert!(status_cache
            .read()
            .unwrap()
            .get_status(key2, &blockhash1, &ancestors2)
            .is_some());
    }
    #[test]
    fn test_shuttle_clear_slots_blockhash_overlap_random() {
        shuttle::check_random(
            do_test_shuttle_clear_slots_blockhash_overlap,
            CLEAR_RANDOM_ITERATIONS,
        );
    }

    #[test]
    fn test_shuttle_clear_slots_blockhash_overlap_dfs() {
        shuttle::check_dfs(
            do_test_shuttle_clear_slots_blockhash_overlap,
            CLEAR_DFS_ITERATIONS,
        );
    }

    // unlike clear_slot_entries(), purge_slots() can't overlap with regular blockhashes since
    // they'd have expired by the time roots are old enough to be purged. However, nonces don't
    // expire, so they can overlap.
    fn do_test_shuttle_purge_nonce_overlap() {
        let status_cache = Arc::new(BankStatusCache::default());
        // fill the cache so that the next add_root() will purge the oldest root
        for i in 0..MAX_CACHE_ENTRIES {
            status_cache.write().unwrap().add_root(i as u64);
        }

        let blockhash1 = Hash::new_from_array([1; 32]);

        let key1 = Hash::new_from_array([3; 32]);
        let key2 = Hash::new_from_array([4; 32]);

        // this slot/key is going to get purged when the th_purge thread calls add_root()
        status_cache
            .write()
            .unwrap()
            .insert(&blockhash1, key1, 0, ());

        let th_purge = shuttle::thread::spawn({
            let status_cache = status_cache.clone();
            move || {
                status_cache
                    .write()
                    .unwrap()
                    .add_root(MAX_CACHE_ENTRIES as Slot + 1);
            }
        });

        let th_insert = shuttle::thread::spawn({
            let status_cache = status_cache.clone();
            move || {
                // insert an entry for a blockhash that gets concurrently purged
                status_cache.write().unwrap().insert(
                    &blockhash1,
                    key2,
                    MAX_CACHE_ENTRIES as Slot + 2,
                    (),
                );
            }
        });
        th_purge.join().unwrap();
        th_insert.join().unwrap();

        let mut ancestors2 = Ancestors::default();
        ancestors2.insert(MAX_CACHE_ENTRIES as Slot + 2, 0);

        assert!(status_cache
            .read()
            .unwrap()
            .get_status(key1, &blockhash1, &ancestors2)
            .is_none());
        assert!(status_cache
            .read()
            .unwrap()
            .get_status(key2, &blockhash1, &ancestors2)
            .is_some());
    }

    #[test]
    fn test_shuttle_purge_nonce_overlap_random() {
        shuttle::check_random(do_test_shuttle_purge_nonce_overlap, PURGE_RANDOM_ITERATIONS);
    }

    #[test]
    fn test_shuttle_purge_nonce_overlap_dfs() {
        shuttle::check_dfs(do_test_shuttle_purge_nonce_overlap, PURGE_DFS_ITERATIONS);
    }

    fn do_test_shuttle_concurrent_inserts() {
        let status_cache = Arc::new(BankStatusCache::default());
        let blockhash1 = Hash::new_from_array([42; 32]);
        let blockhash2 = Hash::new_from_array([43; 32]);
        const N_INSERTS: u8 = 50;

        let mut handles = Vec::with_capacity(N_INSERTS as usize);
        for i in 0..N_INSERTS {
            let status_cache = status_cache.clone();
            let slot = (i % 3) + 1;
            let bh = if i % 2 == 0 { blockhash1 } else { blockhash2 };
            handles.push(shuttle::thread::spawn(move || {
                let key = Hash::new_from_array([i; 32]);
                status_cache
                    .write()
                    .unwrap()
                    .insert(&bh, key, slot as Slot, ());
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut ancestors = Ancestors::default();
        ancestors.insert(1, 0);
        ancestors.insert(2, 0);
        ancestors.insert(3, 0);

        // verify all 100 inserts are visible
        for i in 0..N_INSERTS {
            let key = Hash::new_from_array([i; 32]);
            let bh = if i % 2 == 0 { blockhash1 } else { blockhash2 };
            assert!(
                status_cache
                    .read()
                    .unwrap()
                    .get_status(key, &bh, &ancestors)
                    .is_some(),
                "missing key {}",
                i
            );
        }
    }

    #[test]
    fn test_shuttle_concurrent_inserts_dfs() {
        shuttle::check_dfs(do_test_shuttle_concurrent_inserts, INSERT_DFS_ITERATIONS);
    }

    #[test]
    fn test_shuttle_concurrent_inserts_random() {
        shuttle::check_random(do_test_shuttle_concurrent_inserts, INSERT_RANDOM_ITERATIONS);
    }
}
