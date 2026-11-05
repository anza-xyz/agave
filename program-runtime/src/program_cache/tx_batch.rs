use {
    crate::program_cache::entry::ProgramCacheEntry, solana_clock::Slot, solana_pubkey::Pubkey,
    solana_svm_type_overrides::sync::Arc, std::collections::HashMap,
};

/// Local view into [ProgramCache] which was extracted for a specific TX batch.
///
/// This isolation enables the global [ProgramCache] to continue to evolve (e.g. evictions),
/// while the TX batch is guaranteed it will continue to find all the programs it requires.
/// For program management instructions this also buffers them until their transaction succeeds,
/// at which point they are merged into the batch's own entries.
///
/// [ProgramCache]: crate::program_cache::global::ProgramCache
#[derive(Clone, Debug, Default)]
pub struct ProgramCacheForTxBatch {
    /// K: The program ID
    /// V: A cached program entry valid only for the batch
    entries: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    /// Program entries modified during the transaction batch.
    ///
    /// These are buffered separately in case a transaction fails, but one of
    /// its earlier instructions has modified a program.
    ///
    /// It is folded into the primary index after the transaction is confirmed
    /// to succeed.
    modified_entries: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    /// The slot of this cache's corresponding transaction batch.
    slot: Slot,
    /// Whether any programs were merged into the primary index.
    merged_modified: bool,
}

impl ProgramCacheForTxBatch {
    pub fn new(slot: Slot) -> Self {
        Self {
            entries: HashMap::new(),
            modified_entries: HashMap::new(),
            slot,
            merged_modified: false,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn merged_modified(&self) -> bool {
        self.merged_modified
    }

    /// Refill the cache with a single entry. It's typically called during transaction loading, and
    /// transaction processing (for program management instructions).
    /// It replaces the existing entry (if any) with the provided entry.
    pub fn replenish(&mut self, key: Pubkey, entry: Arc<ProgramCacheEntry>) {
        self.entries.insert(key, entry);
    }

    /// Store an entry in `modified_entries` for a program modified during the
    /// transaction batch.
    pub fn store_modified_entry(&mut self, key: Pubkey, entry: Arc<ProgramCacheEntry>) {
        self.modified_entries.insert(key, entry);
    }

    /// Drain the program cache's modified entries, returning the owned
    /// collection.
    pub fn drain_modified_entries(&mut self) -> HashMap<Pubkey, Arc<ProgramCacheEntry>> {
        std::mem::take(&mut self.modified_entries)
    }

    pub fn find(&self, key: &Pubkey) -> Option<Arc<ProgramCacheEntry>> {
        // First lookup the cache of the programs modified by the current
        // transaction. If not found, lookup the cache of the cache of the
        // programs that are loaded for the transaction batch.
        self.modified_entries
            .get(key)
            .or_else(|| self.entries.get(key))
            .map(|entry| {
                if entry.is_implicit_delay_visibility_tombstone(self.slot) {
                    // Found a program entry on the current fork, but it's not effective
                    // yet. It indicates that the program has delayed visibility. Return
                    // the tombstone to reflect that.
                    Arc::new(ProgramCacheEntry::new_delay_visibility_tombstone(
                        entry.deployment_slot,
                        entry.account_owner,
                        Arc::clone(&entry.stats),
                    ))
                } else {
                    entry.clone()
                }
            })
    }

    /// Merge the provided entries into the primary index.
    pub fn merge(&mut self, entries: &HashMap<Pubkey, Arc<ProgramCacheEntry>>) {
        entries.iter().for_each(|(key, entry)| {
            self.merged_modified = true;
            self.replenish(*key, entry.clone());
        })
    }

    /// Remove an entry from the `entries` list.
    /// Note: DOES NOT remove modified entries!
    pub fn remove_entry(&mut self, key: &Pubkey) {
        self.entries.remove(key);
    }
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl ProgramCacheForTxBatch {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up `entries` directly, without the delay visibility rewrite
    /// `find` performs, so a test can see the entry as it was stored.
    pub fn get_entry_for_tests(&self, key: &Pubkey) -> Option<&Arc<ProgramCacheEntry>> {
        self.entries.get(key)
    }

    pub fn set_slot_for_tests(&mut self, slot: Slot) {
        self.slot = slot;
    }
}
