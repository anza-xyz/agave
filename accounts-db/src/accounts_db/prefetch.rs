//! Background loads that warm the read cache ahead of the caller's critical path.

use {
    super::{AccountsDb, PopulateReadCache, stats::PrefetchAccountsStats},
    crate::ancestors::Ancestors,
    rayon::prelude::*,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        collections::HashSet,
        sync::{Arc, Mutex, atomic::Ordering},
    },
};

#[derive(Debug, Default)]
pub(crate) struct AccountsPrefetcher {
    /// Pubkeys whose prefetch has been dispatched and has not finished
    outstanding: Mutex<HashSet<Pubkey, PubkeyHasherBuilder>>,
    /// Kept apart from `load_account_stats` so a prefetch's own storage read is not counted
    /// as a miss on the critical path
    pub(crate) stats: PrefetchAccountsStats,
}

impl AccountsPrefetcher {
    /// Marks `pubkeys` outstanding, returning the ones that were not already
    fn mark_outstanding(&self, pubkeys: Vec<Pubkey>) -> Vec<Pubkey> {
        let mut outstanding = self.outstanding.lock().unwrap();
        pubkeys
            .into_iter()
            .filter(|pubkey| outstanding.insert(*pubkey))
            .collect()
    }

    fn clear_outstanding(&self, pubkeys: &[Pubkey]) {
        let mut outstanding = self.outstanding.lock().unwrap();
        for pubkey in pubkeys {
            outstanding.remove(pubkey);
        }
    }
}

impl AccountsDb {
    /// Loads `pubkeys` from storage off this thread and stores them in the read cache, so that
    /// a later load on the caller's critical path finds them there.
    ///
    /// `pubkeys` is in the caller's order, which is the order the accounts are about to be used.
    /// The caller's only cost is handing it to the background pool, where everything else runs.
    /// Pubkeys that either cache already holds are dropped: a write-cache version shadows
    /// storage, and a read-cache version is already warm. So are pubkeys an earlier prefetch
    /// is still loading. The rest go through `do_load`, whose `PopulateReadCache::True` path
    /// stores a version only while it is still the newest in the index.
    ///
    /// The loads run against the rooted view rather than any fork's. The read cache only ever
    /// holds the newest version in storage, so a fork's ancestors would not change what gets
    /// cached, and loading against a snapshot of them here is not safe: this call does not keep
    /// the bank they came from alive, and `do_load` panics when it reaches the index entry of a
    /// minor fork whose storages were purged out from under it.
    ///
    /// Fire and forget. The caller is not told when, or whether, a load lands: a prefetch that
    /// finds nothing, races a flush, or is evicted before the account is used is wasted work
    /// and nothing worse.
    pub fn prefetch_accounts(self: &Arc<Self>, pubkeys: Vec<Pubkey>) {
        if pubkeys.is_empty() {
            return;
        }
        let accounts_db = Arc::clone(self);
        // clean is the only other user of this pool and it rarely runs; a prefetch that does
        // queue behind it just lands too late to help, which is where it started
        self.thread_pool_background.spawn(move || {
            let to_load = accounts_db.filter_prefetch(pubkeys);
            accounts_db.load_prefetch(&to_load);
        });
    }

    /// Drops the pubkeys a cache holds or an earlier prefetch is loading, and marks the rest
    /// outstanding
    fn filter_prefetch(&self, pubkeys: Vec<Pubkey>) -> Vec<Pubkey> {
        let prefetcher = &self.prefetcher;
        let mut num_cached = 0;
        let uncached: Vec<_> = pubkeys
            .into_iter()
            .filter(|pubkey| {
                let is_cached = self.accounts_cache.contains_pubkey(pubkey)
                    || self.read_only_accounts_cache.contains_key(pubkey);
                num_cached += u64::from(is_cached);
                !is_cached
            })
            .collect();
        let num_uncached = uncached.len();
        let to_load = prefetcher.mark_outstanding(uncached);

        prefetcher
            .stats
            .num_skipped_cached
            .fetch_add(num_cached, Ordering::Relaxed);
        prefetcher
            .stats
            .num_skipped_outstanding
            .fetch_add((num_uncached - to_load.len()) as u64, Ordering::Relaxed);
        prefetcher
            .stats
            .num_issued
            .fetch_add(to_load.len() as u64, Ordering::Relaxed);
        to_load
    }

    /// Loads `to_load` into the read cache, then clears it from the outstanding set
    fn load_prefetch(&self, to_load: &[Pubkey]) {
        let prefetcher = &self.prefetcher;
        let rooted_ancestors = Ancestors::default();
        to_load.par_iter().for_each(|pubkey| {
            self.do_load(
                &rooted_ancestors,
                pubkey,
                PopulateReadCache::True,
                None::<fn(_, &_, _) -> _>,
                &prefetcher.stats.loads,
            );
        });
        prefetcher.clear_outstanding(to_load);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_outstanding_dedupes_until_cleared() {
        let prefetcher = AccountsPrefetcher::default();
        let first = Pubkey::new_unique();
        let second = Pubkey::new_unique();

        assert_eq!(prefetcher.mark_outstanding(vec![first]), vec![first]);
        // `first` is still in flight, so only `second` is dispatched
        assert_eq!(
            prefetcher.mark_outstanding(vec![first, second]),
            vec![second]
        );

        prefetcher.clear_outstanding(&[first]);
        assert_eq!(
            prefetcher.mark_outstanding(vec![first, second]),
            vec![first]
        );
    }
}
