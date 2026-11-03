//! Background loads that warm the read cache ahead of the caller's critical path.
//!
//! One thread does all of it. It takes pubkeys from its queue, drops those a cache holds or a
//! prefetch is already loading, looks the rest up in the index and reads them from storage; for
//! each finished read it stores the account in the read cache. On io_uring many reads are in
//! flight at once; without io_uring there is no prefetch.
//!
//! The queue holds pubkeys, not batches: callers write their pubkeys straight into its slots, and
//! the thread reads them in place, so neither allocates.
//!
//! The thread alone owns the set of prefetches in flight, so the set needs no lock.
//!
//! With nothing in flight, the thread blocks on its queue, so pubkeys start at once, at the cost
//! of the caller waking it. With reads in flight it waits on the ring instead, so a caller has
//! nothing to wake: its pubkeys wait for the next completion or a short timeout.

// The io_uring prefetcher is the only one; without it, the db's half of the protocol is unused
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
mod ring_prefetcher;

use {
    super::{
        AccountsDb, LoadedAccountAccessor,
        stats::{PrefetchAccountsStats, PrefetchAccountsStatsSub},
    },
    crate::{ancestors::Ancestors, is_zero_lamport::IsZeroLamport},
    shaq::mpmc::{self, Consumer, Producer},
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        collections::HashSet,
        fmt,
        sync::{
            Arc, Weak,
            atomic::{AtomicBool, Ordering},
        },
    },
};
#[cfg(target_os = "linux")]
use {log::warn, ring_prefetcher::RingPrefetcher, std::thread};

/// Pubkeys waiting for the prefetch thread; once full, a caller's pubkeys are dropped. A few ms
/// of backlog: pubkeys queued longer are likely loaded on the critical path first
const MAX_QUEUED_PUBKEYS: usize = 16 * 1024;

pub(crate) struct AccountsPrefetcher {
    /// `None` when there is no prefetch thread
    producer: Option<Producer<Pubkey>>,
    /// Set on drop: the queue has no disconnect, so this is what tells the thread to exit
    exit: Arc<AtomicBool>,
    /// Kept apart from `load_account_stats` so a prefetch's own storage read is not counted
    /// as a miss on the critical path
    pub(crate) stats: PrefetchAccountsStats,
}

impl fmt::Debug for AccountsPrefetcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountsPrefetcher")
            .field("has_thread", &self.producer.is_some())
            .field("stats", &self.stats)
            .finish()
    }
}

impl AccountsPrefetcher {
    /// Starts the prefetch thread, where there is io_uring; elsewhere there is no prefetch, and
    /// every pubkey is dropped as disconnected.
    ///
    /// The thread holds only a `Weak` to `accounts_db`, so it never keeps it alive, and first
    /// touches it on its first pubkeys, which cannot be queued before the `Arc` exists. Dropping
    /// this sets `exit`, and the thread exits the next time it finds its queue empty.
    pub(super) fn new(accounts_db: Weak<AccountsDb>) -> Self {
        let (producer, consumer) = mpmc::pair(MAX_QUEUED_PUBKEYS).expect("prefetch queue");
        let exit = Arc::new(AtomicBool::new(false));
        let has_thread = start_prefetch_thread(accounts_db, consumer, Arc::clone(&exit));
        Self {
            producer: has_thread.then_some(producer),
            exit,
            stats: PrefetchAccountsStats::default(),
        }
    }
}

impl Drop for AccountsPrefetcher {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
    }
}

/// Returns whether it started the thread
#[cfg(target_os = "linux")]
fn start_prefetch_thread(
    accounts_db: Weak<AccountsDb>,
    consumer: Consumer<Pubkey>,
    exit: Arc<AtomicBool>,
) -> bool {
    if !agave_io_uring::io_uring_supported() {
        warn!("no prefetch: io_uring is unsupported");
        return false;
    }
    thread::Builder::new()
        .name("solAcctPrefetch".to_string())
        .spawn(move || match RingPrefetcher::new(accounts_db) {
            Ok(ring_prefetcher) => ring_prefetcher.run(&consumer, &exit),
            Err(err) => warn!("no prefetch: {err}"),
        })
        .expect("spawn prefetch thread");
    true
}

/// Without io_uring there is no prefetch
#[cfg(not(target_os = "linux"))]
fn start_prefetch_thread(
    _accounts_db: Weak<AccountsDb>,
    _consumer: Consumer<Pubkey>,
    _exit: Arc<AtomicBool>,
) -> bool {
    false
}

impl AccountsDb {
    /// Loads `pubkeys` from storage on the prefetch thread and stores them in the read cache,
    /// so a later load on the caller's critical path finds them there. Calls should be in the
    /// order the accounts are about to be used, e.g. one per transaction's keys.
    ///
    /// Fire and forget: the caller only copies `pubkeys` into the queue, and they are dropped
    /// when it is full or there is no prefetch thread (no io_uring). A prefetch that lands late,
    /// or not at all, costs the caller nothing.
    ///
    /// Loads use the rooted view, not a fork's: the read cache only holds the newest stored
    /// version, and this call does not keep a fork's bank alive.
    pub fn prefetch_accounts(&self, pubkeys: &[Pubkey]) {
        let stats = &self.prefetcher.stats;
        let Some(producer) = &self.prefetcher.producer else {
            stats
                .num_disconnected
                .fetch_add(pubkeys.len() as u64, Ordering::Relaxed);
            return;
        };
        if !producer.try_write_slice(pubkeys) {
            stats
                .num_dropped
                .fetch_add(pubkeys.len() as u64, Ordering::Relaxed);
        }
    }
}

/// A read the prefetch thread is done with: the account, or `None` when there is none to store
type FinishedRead = (Pubkey, Slot, Option<AccountSharedData>);

/// Pubkeys a prefetch is loading: each one `start` returns is in flight until `finish`
#[derive(Default)]
struct PrefetchesInFlight(HashSet<Pubkey, PubkeyHasherBuilder>);

impl PrefetchesInFlight {
    /// Returns the slot and storage of `pubkey`'s newest version if no cache holds it and no
    /// prefetch is loading it, marking it in flight
    fn start(
        &mut self,
        accounts_db: &AccountsDb,
        pubkey: Pubkey,
        stats_sub: &mut PrefetchAccountsStatsSub,
    ) -> Option<(Slot, LoadedAccountAccessor)> {
        if accounts_db.accounts_cache.contains_pubkey(&pubkey)
            || accounts_db.read_only_accounts_cache.contains_key(&pubkey)
        {
            stats_sub.num_skipped_cached += 1;
            return None;
        }
        if !self.0.insert(pubkey) {
            stats_sub.num_skipped_outstanding += 1;
            return None;
        }
        let Some((slot, accessor)) =
            accounts_db.get_account_accessor_from_index(&pubkey, &Ancestors::default())
        else {
            self.0.remove(&pubkey);
            stats_sub.num_not_in_index += 1;
            return None;
        };
        stats_sub.num_issued += 1;
        Some((slot, accessor))
    }

    /// Stores each finished read's account if its version is still the newest, and clears its
    /// pubkey either way, or it would never be prefetched again
    fn finish(
        &mut self,
        accounts_db: &AccountsDb,
        finished: impl IntoIterator<Item = FinishedRead>,
        stats_sub: &mut PrefetchAccountsStatsSub,
    ) {
        for (pubkey, slot, account) in finished {
            // Skip zero lamport accounts, as `do_load` does
            if let Some(account) = account
                && !account.is_zero_lamport()
            {
                // held already means a load on the critical path beat this read to it
                let already_cached = accounts_db.read_only_accounts_cache.contains_key(&pubkey);
                if accounts_db.store_in_read_cache_if_newest(&pubkey, slot, &account) {
                    stats_sub.num_stored += 1;
                    if already_cached {
                        stats_sub.num_stored_already_cached += 1;
                    }
                }
            }
            self.0.remove(&pubkey);
        }
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_prefetch_skips_in_flight() {
        let db = AccountsDb::default_for_tests();
        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::new(42, 1, &Pubkey::default());
        db.store_for_tests((0, &[(&pubkey, &account)][..]));
        db.add_root_and_flush_write_cache(0);
        let mut in_flight = PrefetchesInFlight::default();
        let mut stats_sub = PrefetchAccountsStatsSub::default();

        assert!(in_flight.start(&db, pubkey, &mut stats_sub).is_some());

        // still in flight, so not started again
        assert!(in_flight.start(&db, pubkey, &mut stats_sub).is_none());
        assert_eq!(stats_sub.num_issued, 1);
        assert_eq!(stats_sub.num_skipped_outstanding, 1);

        // once finished, it is started again
        in_flight.finish(&db, [(pubkey, 0, None)], &mut stats_sub);
        assert!(in_flight.start(&db, pubkey, &mut stats_sub).is_some());
    }

    #[test]
    fn test_start_prefetch_not_in_index() {
        let db = AccountsDb::default_for_tests();
        let pubkey = Pubkey::new_unique();
        let mut in_flight = PrefetchesInFlight::default();
        let mut stats_sub = PrefetchAccountsStatsSub::default();

        assert!(in_flight.start(&db, pubkey, &mut stats_sub).is_none());
        // nothing to read, so nothing is left in flight
        assert!(in_flight.is_empty());
        assert_eq!(stats_sub.num_not_in_index, 1);
    }
}
