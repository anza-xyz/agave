//! Pubkey index for accounts held in the write cache.

#[cfg(feature = "bbtree-accounts-cache-poc")]
use bbtree::BBTreeMap;
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::bank_id::BankId,
    dashmap::{DashMap, mapref::entry::Entry},
    solana_pubkey::Pubkey,
    std::{
        fmt::Debug,
        sync::atomic::{AtomicU64, Ordering},
    },
};

/// Result of decrementing one pubkey's cache reference count.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountsCacheIndexRemoval {
    Missing,
    Retained,
    Removed,
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
trait AccountsCacheIndexBackend: Debug + Default {
    /// Inserts a reference, returning whether this is a new pubkey.
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId) -> bool;

    /// Decrements a reference and removes the pubkey when its count reaches zero.
    fn remove(&self, pubkey: Pubkey) -> AccountsCacheIndexRemoval;

    /// Returns the greatest bank id observed for this pubkey.
    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId>;

    /// Returns whether the pubkey currently has any cache references.
    fn contains(&self, pubkey: &Pubkey) -> bool;

    /// Returns every pubkey currently in the index.
    fn pubkeys(&self) -> Vec<Pubkey>;
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Debug, Default)]
struct DashMapAccountsCacheIndexBackend {
    entries: DashMap<Pubkey, (BankId, u32), ahash::RandomState>,
}

impl AccountsCacheIndexBackend for DashMapAccountsCacheIndexBackend {
    #[inline]
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId) -> bool {
        match self.entries.entry(*pubkey) {
            Entry::Occupied(mut entry) => {
                let (stored_bank_id, ref_count) = entry.get_mut();
                *stored_bank_id = bank_id.max(*stored_bank_id);
                *ref_count += 1;
                false
            }
            Entry::Vacant(entry) => {
                entry.insert((bank_id, 1));
                true
            }
        }
    }

    #[inline]
    fn remove(&self, pubkey: Pubkey) -> AccountsCacheIndexRemoval {
        let Entry::Occupied(mut entry) = self.entries.entry(pubkey) else {
            return AccountsCacheIndexRemoval::Missing;
        };
        let (_, ref_count) = entry.get_mut();
        *ref_count -= 1;
        if *ref_count == 0 {
            entry.remove_entry();
            AccountsCacheIndexRemoval::Removed
        } else {
            AccountsCacheIndexRemoval::Retained
        }
    }

    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId> {
        self.entries.get(pubkey).map(|entry| entry.0)
    }

    fn contains(&self, pubkey: &Pubkey) -> bool {
        self.entries.contains_key(pubkey)
    }

    fn pubkeys(&self) -> Vec<Pubkey> {
        self.entries.iter().map(|entry| *entry.key()).collect()
    }
}

#[cfg(feature = "bbtree-accounts-cache-poc")]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Debug, Default)]
struct BbtreeAccountsCacheIndexBackend {
    entries: BBTreeMap<[u8; 32], (BankId, u32)>,
}

#[cfg(feature = "bbtree-accounts-cache-poc")]
impl AccountsCacheIndexBackend for BbtreeAccountsCacheIndexBackend {
    #[inline]
    fn insert(&self, pubkey: &Pubkey, bank_id: BankId) -> bool {
        let update = self.entries.update_or_insert(
            *pubkey.as_array(),
            (bank_id, 1),
            |_key, stored, incoming| {
                let next_ref_count = stored.1.checked_add(incoming.1)?;
                stored.0 = incoming.0.max(stored.0);
                stored.1 = next_ref_count;
                Some(())
            },
        );

        match update {
            Ok(None) => true,
            Ok(Some(Some(()))) => false,
            Ok(Some(None)) => panic!("cache index reference count overflow for {pubkey}"),
            Err(_) => panic!("failed to allocate cache index entry for {pubkey}"),
        }
    }

    #[inline]
    fn remove(&self, pubkey: Pubkey) -> AccountsCacheIndexRemoval {
        let key = pubkey.to_bytes();
        let remove_entry = match self.entries.update(&key, |_key, (_, ref_count)| {
            let next_ref_count = ref_count.checked_sub(1)?;
            *ref_count = next_ref_count;
            Some(next_ref_count == 0)
        }) {
            None => return AccountsCacheIndexRemoval::Missing,
            Some(Some(remove_entry)) => remove_entry,
            Some(None) => panic!("cache index reference count underflow for {pubkey}"),
        };

        if !remove_entry {
            return AccountsCacheIndexRemoval::Retained;
        }

        if self
            .entries
            .remove_if(&key, |_key, (_, ref_count)| *ref_count == 0)
            .is_some()
        {
            AccountsCacheIndexRemoval::Removed
        } else {
            // A concurrent insert can revive a zero-count entry before remove_if.
            AccountsCacheIndexRemoval::Retained
        }
    }

    fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId> {
        self.entries
            .get_copied(pubkey.as_array())
            .map(|(bank_id, _ref_count)| bank_id)
    }

    fn contains(&self, pubkey: &Pubkey) -> bool {
        self.entries.contains_key(pubkey.as_array())
    }

    fn pubkeys(&self) -> Vec<Pubkey> {
        self.entries
            .range_cloned::<[u8; 32], _>(..)
            .map(|(key, _value)| Pubkey::new_from_array(key))
            .collect()
    }
}

#[cfg(not(feature = "bbtree-accounts-cache-poc"))]
pub(crate) type AccountsCacheIndex = AccountsCacheIndexImpl<DashMapAccountsCacheIndexBackend>;
#[cfg(feature = "bbtree-accounts-cache-poc")]
pub(crate) type AccountsCacheIndex = AccountsCacheIndexImpl<BbtreeAccountsCacheIndexBackend>;

#[cfg(feature = "dev-context-only-utils")]
pub type DashMapAccountsCacheIndex = AccountsCacheIndexImpl<DashMapAccountsCacheIndexBackend>;
#[cfg(all(
    feature = "bbtree-accounts-cache-poc",
    feature = "dev-context-only-utils"
))]
pub type BbtreeAccountsCacheIndex = AccountsCacheIndexImpl<BbtreeAccountsCacheIndexBackend>;

/// Maps each pubkey to (max_bank_id, ref_count) where max_bank_id is the highest bank id that has
/// written the pubkey into the cache, and ref_count is the number of SlotCache entries that
/// currently hold the pubkey. max_bank_id may be stale after a removal; callers must handle a
/// look-up miss on max_bank_id by falling back to scanning all slots in the cache (see load_latest)
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Debug, Default)]
struct AccountsCacheIndexImpl<B: AccountsCacheIndexBackend> {
    backend: B,
    // The number of unique pubkeys in the index, for reporting purposes. This is to avoid counting
    // keys on demand. DashMap would need to lock every shard, while bbtree does not track length.
    num_unique_pubkeys: AtomicU64,
}

impl<B: AccountsCacheIndexBackend> AccountsCacheIndexImpl<B> {
    /// Inserts an entry into the index. If the entry is already present, increase the ref count.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    #[inline]
    pub(crate) fn insert(&self, pubkey: &Pubkey, bank_id: BankId) {
        if self.backend.insert(pubkey, bank_id) {
            self.num_unique_pubkeys.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Decrement the reference count for each pubkey in `pubkeys`. Removes an entry entirely if
    /// the count reaches zero. `max_bank_id` is not updated; it will become stale if the removed
    /// bank is the highest. Returns a vec of pubkeys removed from the index.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    #[inline]
    pub(crate) fn remove(&self, pubkeys: impl IntoIterator<Item = Pubkey>) -> Vec<Pubkey> {
        let mut removed_pubkeys = Vec::new();
        for pubkey in pubkeys {
            match self.backend.remove(pubkey) {
                AccountsCacheIndexRemoval::Missing => {
                    panic!("pubkey {pubkey} not found in cache index during remove");
                }
                AccountsCacheIndexRemoval::Retained => {}
                AccountsCacheIndexRemoval::Removed => {
                    self.num_unique_pubkeys.fetch_sub(1, Ordering::Relaxed);
                    removed_pubkeys.push(pubkey);
                }
            }
        }
        removed_pubkeys
    }

    /// Returns the highest bank id that stored `pubkey` in the cache, or `None` if the pubkey is
    /// not present in the cache. That bank's slot cache may since have been flushed or purged, so
    /// this is only an upper bound on the banks whose slot caches hold `pubkey`.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn max_bank_id_for_pubkey(&self, pubkey: &Pubkey) -> Option<BankId> {
        self.backend.max_bank_id_for_pubkey(pubkey)
    }

    pub(crate) fn contains(&self, pubkey: &Pubkey) -> bool {
        self.backend.contains(pubkey)
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn pubkeys(&self) -> Vec<Pubkey> {
        self.backend.pubkeys()
    }

    pub(crate) fn num_unique_pubkeys(&self) -> u64 {
        self.num_unique_pubkeys.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "bbtree-accounts-cache-poc")]
    use std::sync::Arc;

    fn assert_cache_index_backend_semantics<B: AccountsCacheIndexBackend>() {
        let index = AccountsCacheIndexImpl::<B>::default();
        let pubkey = Pubkey::new_unique();

        index.insert(&pubkey, BankId::new(5));
        index.insert(&pubkey, BankId::new(3));
        assert_eq!(index.max_bank_id_for_pubkey(&pubkey), Some(BankId::new(5)));
        assert_eq!(index.num_unique_pubkeys(), 1);
        assert!(index.contains(&pubkey));
        assert_eq!(index.pubkeys(), vec![pubkey]);

        assert!(index.remove([pubkey]).is_empty());
        assert_eq!(index.max_bank_id_for_pubkey(&pubkey), Some(BankId::new(5)));
        assert_eq!(index.remove([pubkey]), vec![pubkey]);
        assert!(index.max_bank_id_for_pubkey(&pubkey).is_none());
        assert_eq!(index.num_unique_pubkeys(), 0);
        assert!(!index.contains(&pubkey));
        assert!(index.pubkeys().is_empty());
    }

    #[test]
    fn test_dashmap_cache_index_backend_semantics() {
        assert_cache_index_backend_semantics::<DashMapAccountsCacheIndexBackend>();
    }

    #[cfg(feature = "bbtree-accounts-cache-poc")]
    #[test]
    fn test_bbtree_cache_index_backend_semantics() {
        assert_cache_index_backend_semantics::<BbtreeAccountsCacheIndexBackend>();
    }

    #[cfg(feature = "bbtree-accounts-cache-poc")]
    #[test]
    fn test_bbtree_cache_index_remove_insert_race_preserves_reference() {
        const ITERATIONS: usize = 1_000;

        for _ in 0..ITERATIONS {
            let index =
                Arc::new(AccountsCacheIndexImpl::<BbtreeAccountsCacheIndexBackend>::default());
            let pubkey = Pubkey::new_unique();
            index.insert(&pubkey, BankId::new(1));
            let start = Arc::new(std::sync::Barrier::new(3));

            std::thread::scope(|scope| {
                let remove_index = Arc::clone(&index);
                let remove_start = Arc::clone(&start);
                let remove = scope.spawn(move || {
                    remove_start.wait();
                    remove_index.remove([pubkey])
                });

                let insert_index = Arc::clone(&index);
                let insert_start = Arc::clone(&start);
                let insert = scope.spawn(move || {
                    insert_start.wait();
                    insert_index.insert(&pubkey, BankId::new(2));
                });

                start.wait();
                let _removed = remove.join().expect("remove worker should not panic");
                insert.join().expect("insert worker should not panic");
            });

            assert_eq!(index.max_bank_id_for_pubkey(&pubkey), Some(BankId::new(2)));
            assert_eq!(index.num_unique_pubkeys(), 1);
        }
    }
}
