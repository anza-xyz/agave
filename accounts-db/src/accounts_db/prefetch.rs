//! Background loads that warm the read cache ahead of the caller's critical path.
//!
//! One thread does all of it. For each batch it drops pubkeys a cache holds or a prefetch is
//! already loading, looks the rest up in the index and reads them from storage; for each finished
//! read it stores the account in the read cache. On io_uring many reads are in flight at once;
//! where io_uring is unavailable, each read blocks.
//!
//! The thread alone owns the set of outstanding pubkeys, so the set needs no lock.
//!
//! The thread polls its queue rather than blocking on it. A blocked receiver has to be woken by
//! the sender, a syscall on the caller's critical path; a polled one never does.

#[cfg(target_os = "linux")]
mod ring_prefetcher;

use {
    super::{AccountsDb, LoadedAccountAccessor, stats::PrefetchAccountsStats},
    crate::ancestors::Ancestors,
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded},
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        collections::HashSet,
        sync::{Arc, OnceLock, Weak, atomic::Ordering},
        thread,
        time::Duration,
    },
};

/// Batches waiting for the prefetch thread; once full, new batches are dropped
const MAX_QUEUED_BATCHES: usize = 64;
/// With nothing to do, how long the prefetch thread sleeps before checking its queue again
const POLL_INTERVAL: Duration = Duration::from_micros(200);

type Outstanding = HashSet<Pubkey, PubkeyHasherBuilder>;

#[derive(Debug, Default)]
pub(crate) struct AccountsPrefetcher {
    /// Set by the first prefetch, which starts the thread: it needs the `Arc<AccountsDb>`
    batch_sender: OnceLock<Sender<Vec<Pubkey>>>,
    /// Kept apart from `load_account_stats` so a prefetch's own storage read is not counted
    /// as a miss on the critical path
    pub(crate) stats: PrefetchAccountsStats,
}

/// A read of one account from storage
struct Read {
    pubkey: Pubkey,
    slot: Slot,
    accessor: LoadedAccountAccessor,
}

/// What dispatching did with each pubkey, added to the stats once per pass rather than per pubkey
#[derive(Default)]
struct DispatchCounts {
    num_skipped_cached: u64,
    num_skipped_outstanding: u64,
    num_not_in_index: u64,
    num_issued: u64,
}

impl DispatchCounts {
    fn add_to(self, stats: &PrefetchAccountsStats) {
        stats
            .num_skipped_cached
            .fetch_add(self.num_skipped_cached, Ordering::Relaxed);
        stats
            .num_skipped_outstanding
            .fetch_add(self.num_skipped_outstanding, Ordering::Relaxed);
        stats
            .num_not_in_index
            .fetch_add(self.num_not_in_index, Ordering::Relaxed);
        stats
            .num_issued
            .fetch_add(self.num_issued, Ordering::Relaxed);
    }
}

impl AccountsPrefetcher {
    /// Starts the prefetch thread and returns its input.
    ///
    /// The thread holds only a `Weak` to `accounts_db`, so it never keeps it alive. Dropping it
    /// drops the returned sender, and the thread exits once its input disconnects.
    fn start(accounts_db: Weak<AccountsDb>) -> Sender<Vec<Pubkey>> {
        let (batch_sender, batch_receiver) = bounded(MAX_QUEUED_BATCHES);
        thread::Builder::new()
            .name("solAcctPrefetch".to_string())
            .spawn(move || run_prefetcher(accounts_db, batch_receiver))
            .expect("spawn prefetch thread");
        batch_sender
    }
}

fn run_prefetcher(accounts_db: Weak<AccountsDb>, batch_receiver: Receiver<Vec<Pubkey>>) {
    #[cfg(target_os = "linux")]
    if agave_io_uring::io_uring_supported() {
        match ring_prefetcher::RingPrefetcher::new(accounts_db.clone()) {
            Ok(ring_prefetcher) => return ring_prefetcher.run(batch_receiver),
            Err(err) => log::warn!("prefetch falling back to blocking reads: {err}"),
        }
    }
    run_blocking_prefetcher(accounts_db, batch_receiver)
}

/// Reads one account at a time, blocking on each
fn run_blocking_prefetcher(accounts_db: Weak<AccountsDb>, batch_receiver: Receiver<Vec<Pubkey>>) {
    let mut outstanding = Outstanding::default();
    loop {
        let pubkeys = match batch_receiver.try_recv() {
            Ok(pubkeys) => pubkeys,
            Err(TryRecvError::Empty) => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(TryRecvError::Disconnected) => break,
        };
        let Some(accounts_db) = accounts_db.upgrade() else {
            break;
        };
        let mut counts = DispatchCounts::default();
        for pubkey in pubkeys {
            let Some(Read {
                pubkey,
                slot,
                mut accessor,
            }) = accounts_db.read_for_prefetch(pubkey, &mut outstanding, &mut counts)
            else {
                continue;
            };
            let account =
                accessor.check_and_get_loaded_account_shared_data(None::<fn(_, &_, _) -> _>);
            accounts_db.finish_prefetch(pubkey, slot, account, &mut outstanding);
        }
        counts.add_to(&accounts_db.prefetcher.stats);
    }
}

impl AccountsDb {
    /// Loads `pubkeys` from storage off this thread and stores them in the read cache, so that
    /// a later load on the caller's critical path finds them there.
    ///
    /// `pubkeys` is in the caller's order, which is the order the accounts are about to be used.
    /// The caller's only cost is queueing it for the prefetch thread; when that queue is full
    /// the batch is dropped. Pubkeys that either cache already holds are dropped: a write-cache
    /// version shadows storage, and a read-cache version is already warm. So are pubkeys an
    /// earlier prefetch is still loading. The rest are read from storage and stored only while
    /// their version is still the newest in the index.
    ///
    /// The loads run against the rooted view rather than any fork's. The read cache only ever
    /// holds the newest version in storage, so a fork's ancestors would not change what gets
    /// cached, and loading against a snapshot of them here is not safe: this call does not keep
    /// the bank they came from alive, and the index entry of a minor fork may lose its storage
    /// before the read.
    ///
    /// Fire and forget. The caller is not told when, or whether, a load lands: a prefetch that
    /// finds nothing, races a flush, or is evicted before the account is used is wasted work
    /// and nothing worse.
    pub fn prefetch_accounts(self: &Arc<Self>, pubkeys: Vec<Pubkey>) {
        if pubkeys.is_empty() {
            return;
        }
        let batch_sender = self
            .prefetcher
            .batch_sender
            .get_or_init(|| AccountsPrefetcher::start(Arc::downgrade(self)));
        if let Err(TrySendError::Full(_)) = batch_sender.try_send(pubkeys) {
            self.prefetcher
                .stats
                .num_batches_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns a read of `pubkey` if no cache holds it and no prefetch is loading it, marking it
    /// outstanding
    fn read_for_prefetch(
        &self,
        pubkey: Pubkey,
        outstanding: &mut Outstanding,
        counts: &mut DispatchCounts,
    ) -> Option<Read> {
        if self.accounts_cache.contains_pubkey(&pubkey)
            || self.read_only_accounts_cache.contains_key(&pubkey)
        {
            counts.num_skipped_cached += 1;
            return None;
        }
        if !outstanding.insert(pubkey) {
            counts.num_skipped_outstanding += 1;
            return None;
        }
        let Some((slot, accessor)) =
            self.get_account_accessor_from_index(&pubkey, &Ancestors::default())
        else {
            outstanding.remove(&pubkey);
            counts.num_not_in_index += 1;
            return None;
        };
        counts.num_issued += 1;
        Some(Read {
            pubkey,
            slot,
            accessor,
        })
    }

    /// Stores a finished read's account if its version is still the newest, and clears its
    /// pubkey from `outstanding` either way, or it would never be prefetched again
    fn finish_prefetch(
        &self,
        pubkey: Pubkey,
        slot: Slot,
        account: Option<AccountSharedData>,
        outstanding: &mut Outstanding,
    ) {
        if let Some(account) = account
            && self.store_in_read_cache_if_newest(&pubkey, slot, &account)
        {
            self.prefetcher
                .stats
                .num_stored
                .fetch_add(1, Ordering::Relaxed);
        }
        outstanding.remove(&pubkey);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_for_prefetch_skips_outstanding() {
        let db = AccountsDb::default_for_tests();
        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::new(42, 1, AccountSharedData::default().owner());
        db.store_for_tests((0, &[(&pubkey, &account)][..]));
        db.add_root_and_flush_write_cache(0);
        let mut outstanding = Outstanding::default();
        let mut counts = DispatchCounts::default();

        assert!(
            db.read_for_prefetch(pubkey, &mut outstanding, &mut counts)
                .is_some()
        );

        // still outstanding, so not read again
        assert!(
            db.read_for_prefetch(pubkey, &mut outstanding, &mut counts)
                .is_none()
        );
        assert_eq!(counts.num_issued, 1);
        assert_eq!(counts.num_skipped_outstanding, 1);

        // once finished, it is read again
        db.finish_prefetch(pubkey, 0, None, &mut outstanding);
        assert!(
            db.read_for_prefetch(pubkey, &mut outstanding, &mut counts)
                .is_some()
        );
    }

    #[test]
    fn test_read_for_prefetch_not_in_index() {
        let db = AccountsDb::default_for_tests();
        let pubkey = Pubkey::new_unique();
        let mut outstanding = Outstanding::default();
        let mut counts = DispatchCounts::default();

        assert!(
            db.read_for_prefetch(pubkey, &mut outstanding, &mut counts)
                .is_none()
        );
        // nothing is in flight, so nothing is left outstanding
        assert!(outstanding.is_empty());
        assert_eq!(counts.num_not_in_index, 1);
    }
}
