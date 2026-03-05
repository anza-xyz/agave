use {
    dashmap::{DashMap, mapref::entry::Entry as DashMapEntry},
    log::*,
    solana_pubkey::Pubkey,
    solana_time_utils::AtomicInterval,
    std::{
        collections::HashSet,
        fmt::Debug,
        sync::{
            RwLock,
            atomic::{AtomicU64, Ordering},
        },
    },
};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct AccountSecondaryIndexes {
    pub keys: Option<AccountSecondaryIndexesIncludeExclude>,
    pub indexes: HashSet<AccountIndex>,
}

impl AccountSecondaryIndexes {
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }
    pub fn contains(&self, index: &AccountIndex) -> bool {
        self.indexes.contains(index)
    }
    pub fn include_key(&self, key: &Pubkey) -> bool {
        match &self.keys {
            Some(options) => options.exclude ^ options.keys.contains(key),
            None => true, // include all keys
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AccountSecondaryIndexesIncludeExclude {
    pub exclude: bool,
    pub keys: HashSet<Pubkey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AccountIndex {
    ProgramId,
    SplTokenMint,
    SplTokenOwner,
}

#[derive(Debug, Clone, Copy)]
pub enum IndexKey {
    ProgramId(Pubkey),
    SplTokenMint(Pubkey),
    SplTokenOwner(Pubkey),
}

// The only cases where an inner key should map to a different outer key is
// if the key had different account data for the indexed key across different
// slots. As this is rare, it should be ok to use a Vec here over a HashSet, even
// though we are running some key existence checks.
type SecondaryReverseIndexEntry = RwLock<Vec<Pubkey>>;

pub trait SecondaryIndexEntry: Debug {
    fn insert_if_not_exists(&self, key: &Pubkey, inner_keys_count: &AtomicU64);
    // Removes a value from the set. Returns whether the value was present in the set.
    fn remove_inner_key(&self, key: &Pubkey) -> bool;
    fn is_empty(&self) -> bool;
    fn keys(&self) -> Vec<Pubkey>;
    fn len(&self) -> usize;
}

#[derive(Debug, Default)]
struct SecondaryIndexStats {
    last_report: AtomicInterval,
    num_inner_keys: AtomicU64,
}

#[derive(Debug, Default)]
pub struct RwLockSecondaryIndexEntry {
    account_keys: RwLock<HashSet<Pubkey>>,
}

impl SecondaryIndexEntry for RwLockSecondaryIndexEntry {
    fn insert_if_not_exists(&self, key: &Pubkey, inner_keys_count: &AtomicU64) {
        if self.account_keys.read().unwrap().contains(key) {
            // the key already exists, so nothing to do here
            return;
        }

        let was_newly_inserted = self.account_keys.write().unwrap().insert(*key);
        if was_newly_inserted {
            inner_keys_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn remove_inner_key(&self, key: &Pubkey) -> bool {
        self.account_keys.write().unwrap().remove(key)
    }

    fn is_empty(&self) -> bool {
        self.account_keys.read().unwrap().is_empty()
    }

    fn keys(&self) -> Vec<Pubkey> {
        self.account_keys.read().unwrap().iter().cloned().collect()
    }

    fn len(&self) -> usize {
        self.account_keys.read().unwrap().len()
    }
}

#[derive(Debug, Default)]
pub struct SecondaryIndex<SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send> {
    metrics_name: &'static str,
    // Map from index keys to index values
    pub index: DashMap<Pubkey, SecondaryIndexEntryType>,
    pub reverse_index: DashMap<Pubkey, SecondaryReverseIndexEntry>,
    stats: SecondaryIndexStats,
}

impl<SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send>
    SecondaryIndex<SecondaryIndexEntryType>
{
    pub fn new(metrics_name: &'static str) -> Self {
        Self {
            metrics_name,
            ..Self::default()
        }
    }

    /// Inserts `inner_key` into `key`'s map.
    pub fn insert(&self, key: &Pubkey, inner_key: &Pubkey) {
        // Note: Always lock the reverse index first, so we synchronize with remove().
        let reverse_index_entry = self.reverse_index.entry(*inner_key).or_default();
        let mut outer_keys = reverse_index_entry.write().unwrap();

        // Now insert into the index.
        // Note, we do this get()-then-unwrap instead of calling entry() directly, because
        // get() is a read lock whereas entry() is a write lock.  We assume `key` already has
        // a map created, so optimize for the common case and only take a read lock.
        self.index
            .get(key)
            .unwrap_or_else(|| self.index.entry(*key).or_default().downgrade())
            .insert_if_not_exists(inner_key, &self.stats.num_inner_keys);

        if !outer_keys.contains(key) {
            outer_keys.push(*key);
        }

        // explicitly drop the locks so we don't hold them while reporting metrics
        drop(outer_keys);
        drop(reverse_index_entry);

        if self.stats.last_report.should_update(1000) {
            datapoint_info!(
                self.metrics_name,
                ("num_secondary_keys", self.index.len(), i64),
                (
                    "num_inner_keys",
                    self.stats.num_inner_keys.load(Ordering::Relaxed),
                    i64
                ),
                ("num_reverse_index_keys", self.reverse_index.len(), i64),
            );
        }
    }

    /// Removes `inner_key` from `outer_key`'s map.
    ///
    /// Must only be called by remove_by_inner_key(), or equiv, that is
    /// holding a lock on self.reverse_index.
    fn remove_index_entries(&self, outer_key: &Pubkey, inner_key: &Pubkey) -> bool {
        let Some(inner_keys) = self.index.get_mut(outer_key) else {
            // we were told that inner_key is in the outer_key map,
            // so the outer_key map should exist!
            panic!(
                "{}: bad index: missing entry for outer_key={outer_key} (inner_key={inner_key})",
                self.metrics_name
            );
        };

        let was_removed = inner_keys.value().remove_inner_key(inner_key);
        if !was_removed {
            // we were told that inner_key is in the outer_key map,
            // so the outer_key map should contain the inner_key!
            panic!(
                "{}: bad index: missing entry for inner_key={inner_key} in map for \
                 outer_key={outer_key}",
                self.metrics_name
            );
        }

        // Before dropping the lock, check if the outer_key map is empty.
        // Because if it is *not* empty, we can skip checking again below.
        let is_outer_key_empty = inner_keys.is_empty();
        drop(inner_keys);

        if is_outer_key_empty {
            // If the outer_key map was empty, we'll check again and remove it if still empty.
            // If it is no longer empty, that is fine, it was re-added, and nothing to do here.
            self.index
                .remove_if(outer_key, |_, inner_keys| inner_keys.is_empty());
        }
        was_removed
    }

    /// Removes `inner_key` from the secondary index.
    pub fn remove_by_inner_key(&self, inner_key: &Pubkey) {
        // Note: Always lock the reverse-index first, so we synchronize with insert().
        let DashMapEntry::Occupied(reverse_index_entry) = self.reverse_index.entry(*inner_key)
        else {
            // if inner_key doesn't exist in the reverse-index, nothing to do here
            return;
        };

        // First go through the reverse-index and remove inner_key from all forward-indexes.
        let num_removed = reverse_index_entry
            .get()
            .write()
            .unwrap()
            .drain(..)
            .map(|outer_key| self.remove_index_entries(&outer_key, inner_key) as u64)
            .sum();

        // And now after removing inner_key from all forward-indexes,
        // remove its entry from the reverse-index.
        reverse_index_entry.remove();

        self.stats
            .num_inner_keys
            .fetch_sub(num_removed, Ordering::Relaxed);
    }

    pub fn get(&self, key: &Pubkey) -> Vec<Pubkey> {
        if let Some(inner_keys_map) = self.index.get(key) {
            inner_keys_map.keys()
        } else {
            vec![]
        }
    }

    /// log top 20 (owner, # accounts) in descending order of # accounts
    pub fn log_contents(&self) {
        let mut entries = self
            .index
            .iter()
            .map(|entry| (entry.value().len(), *entry.key()))
            .collect::<Vec<_>>();
        entries.sort_unstable();
        entries
            .iter()
            .rev()
            .take(20)
            .for_each(|(v, k)| info!("owner: {k}, accounts: {v}"));
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            iter,
            sync::{Arc, atomic::AtomicBool},
            thread,
        },
    };

    // Ensures remove_by_inner() enforces invariant that outer_key must
    // have an entry in forward index.
    #[test]
    #[should_panic(expected = "bad index: missing entry for outer_key=")]
    fn test_remove_by_inner_key_panics_on_stale_reverse_mapping() {
        let secondary_index =
            SecondaryIndex::<RwLockSecondaryIndexEntry>::new("test_secondary_index");
        let outer_key = Pubkey::new_unique();
        let inner_key = Pubkey::new_unique();

        // only add an entry to the reverse index, not the forward index
        secondary_index
            .reverse_index
            .insert(inner_key, RwLock::new(vec![outer_key]));

        secondary_index.remove_by_inner_key(&inner_key);
    }

    // Ensures remove_by_inner() enforces invariant that inner_key must
    // have an entry in the outer_key's forward index map.
    #[test]
    #[should_panic(expected = "bad index: missing entry for inner_key=")]
    fn test_remove_by_inner_key_panics_on_stale_forward_mapping() {
        let secondary_index =
            SecondaryIndex::<RwLockSecondaryIndexEntry>::new("test_secondary_index");
        let inner_key = Pubkey::new_unique();
        let outer_key_1 = Pubkey::new_unique();
        let outer_key_2 = Pubkey::new_unique();

        secondary_index.insert(&outer_key_1, &inner_key);
        secondary_index.insert(&outer_key_2, &inner_key);

        // remove the inner key from the outer key's forward index map
        secondary_index
            .index
            .get(&outer_key_2)
            .unwrap()
            .remove_inner_key(&inner_key);

        secondary_index.remove_by_inner_key(&inner_key);
    }

    /// Ensures concurrent calls to insert() and remove_by_inner() don't race/panic.
    #[test]
    fn test_concurrent_insert_remove() {
        const ITERATIONS: usize = 10_000;
        let secondary_index = Arc::new(SecondaryIndex::<RwLockSecondaryIndexEntry>::new(""));
        let outer_keys: Vec<_> = iter::repeat_with(Pubkey::new_unique).take(3).collect();
        let inner_keys: Vec<_> = iter::repeat_with(Pubkey::new_unique).take(9).collect();
        let mut handles = Vec::new();
        let go = Arc::new(AtomicBool::new(false));

        // spawn inserter threads
        for outer_key in &outer_keys {
            let secondary_index = Arc::clone(&secondary_index);
            let go = Arc::clone(&go);
            let outer_key = *outer_key;
            let inner_keys = inner_keys.clone();
            handles.push(thread::spawn(move || {
                while !go.load(Ordering::Relaxed) {}
                for _ in 0..ITERATIONS {
                    for inner_key in &inner_keys {
                        secondary_index.insert(&outer_key, inner_key);
                    }
                }
            }));
        }

        // spawn remover thread
        {
            let secondary_index = Arc::clone(&secondary_index);
            let go = Arc::clone(&go);
            let inner_keys = inner_keys.clone();
            handles.push(thread::spawn(move || {
                while !go.load(Ordering::Relaxed) {}
                for _ in 0..ITERATIONS {
                    for inner_key in &inner_keys {
                        secondary_index.remove_by_inner_key(inner_key);
                    }
                }
            }));
        }

        go.store(true, Ordering::Relaxed);
        for handle in handles {
            handle.join().unwrap();
        }

        // After all the concurrent insert/removals, try removing everything
        // and ensure final state is consistent.
        for inner_key in &inner_keys {
            secondary_index.remove_by_inner_key(inner_key);
            assert!(secondary_index.reverse_index.get(inner_key).is_none());
        }
        for outer_key in &outer_keys {
            assert!(secondary_index.index.get(outer_key).is_none());
        }
        assert_eq!(
            secondary_index.stats.num_inner_keys.load(Ordering::Relaxed),
            0,
        );
    }
}
