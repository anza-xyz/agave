//! Background loads that warm the read cache ahead of the caller's critical path.
//!
//! Three stages, each on its own thread, connected by channels:
//! - the dispatcher drops pubkeys a cache holds or a prefetch is already loading, looks the
//!   rest up in the index and queues a read for each
//! - the reader keeps many reads in flight on io_uring, and falls back to blocking reads where
//!   io_uring is unavailable
//! - the completer stores each account in the read cache and reports its pubkey done
//!
//! The dispatcher alone owns the set of outstanding pubkeys; the completer reports finished
//! ones back to it over a channel, so the set needs no lock.

#[cfg(target_os = "linux")]
mod ring_reader;

use {
    super::{AccountsDb, LoadedAccountAccessor, stats::PrefetchAccountsStats},
    crate::ancestors::Ancestors,
    crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded},
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        collections::HashSet,
        sync::{Arc, OnceLock, Weak, atomic::Ordering},
        thread,
    },
};

/// Batches waiting for the dispatcher; once full, new batches are dropped
const MAX_QUEUED_BATCHES: usize = 64;
/// Reads waiting for the reader, and finished reads waiting for the completer; once full, the
/// stage feeding the queue waits
const MAX_QUEUED_READS: usize = 4096;

type Outstanding = HashSet<Pubkey, PubkeyHasherBuilder>;

#[derive(Debug, Default)]
pub(crate) struct AccountsPrefetcher {
    /// Set by the first prefetch, which starts the stages: they need the `Arc<AccountsDb>`
    batch_sender: OnceLock<Sender<Vec<Pubkey>>>,
    /// Kept apart from `load_account_stats` so a prefetch's own storage read is not counted
    /// as a miss on the critical path
    pub(crate) stats: PrefetchAccountsStats,
}

/// A read the dispatcher queued for the reader
struct Read {
    pubkey: Pubkey,
    slot: Slot,
    accessor: LoadedAccountAccessor,
}

/// A finished read, for the completer
struct Completion {
    pubkey: Pubkey,
    slot: Slot,
    account: Option<AccountSharedData>,
}

impl AccountsPrefetcher {
    /// Starts the stages and returns the dispatcher's input.
    ///
    /// The stages hold only a `Weak` to `accounts_db`, so they never keep it alive. Dropping it
    /// drops the returned sender, and each stage exits once its input disconnects.
    fn start(accounts_db: Weak<AccountsDb>) -> Sender<Vec<Pubkey>> {
        let (batch_sender, batch_receiver) = bounded(MAX_QUEUED_BATCHES);
        let (read_sender, read_receiver) = bounded(MAX_QUEUED_READS);
        let (completion_sender, completion_receiver) = bounded(MAX_QUEUED_READS);
        // unbounded: the dispatcher drains it only between batches, so a full queue here could
        // stall the completer while the dispatcher waits on the reader. It never holds more
        // than the outstanding set.
        let (done_sender, done_receiver) = unbounded();

        let dispatcher_accounts_db = accounts_db.clone();
        thread::Builder::new()
            .name("solAcctPrefDisp".to_string())
            .spawn(move || {
                run_dispatcher(
                    dispatcher_accounts_db,
                    batch_receiver,
                    read_sender,
                    done_receiver,
                )
            })
            .expect("spawn prefetch dispatcher");
        thread::Builder::new()
            .name("solAcctPrefRead".to_string())
            .spawn(move || run_reader(read_receiver, completion_sender))
            .expect("spawn prefetch reader");
        thread::Builder::new()
            .name("solAcctPrefComp".to_string())
            .spawn(move || run_completer(accounts_db, completion_receiver, done_sender))
            .expect("spawn prefetch completer");
        batch_sender
    }
}

fn run_dispatcher(
    accounts_db: Weak<AccountsDb>,
    batch_receiver: Receiver<Vec<Pubkey>>,
    read_sender: Sender<Read>,
    done_receiver: Receiver<Pubkey>,
) {
    let mut outstanding = Outstanding::default();
    for pubkeys in batch_receiver {
        for pubkey in done_receiver.try_iter() {
            outstanding.remove(&pubkey);
        }
        let Some(accounts_db) = accounts_db.upgrade() else {
            break;
        };
        if !accounts_db.dispatch_prefetch(pubkeys, &mut outstanding, &read_sender) {
            break;
        }
    }
}

fn run_reader(read_receiver: Receiver<Read>, completion_sender: Sender<Completion>) {
    #[cfg(target_os = "linux")]
    if agave_io_uring::io_uring_supported() {
        match ring_reader::RingReader::new(completion_sender.clone()) {
            Ok(ring_reader) => return ring_reader.run(read_receiver),
            Err(err) => log::warn!("prefetch reader falling back to blocking reads: {err}"),
        }
    }
    run_blocking_reader(read_receiver, completion_sender)
}

/// Reads one account at a time, blocking on each
fn run_blocking_reader(read_receiver: Receiver<Read>, completion_sender: Sender<Completion>) {
    for Read {
        pubkey,
        slot,
        mut accessor,
    } in read_receiver
    {
        let account = accessor.check_and_get_loaded_account_shared_data(None::<fn(_, &_, _) -> _>);
        let completion = Completion {
            pubkey,
            slot,
            account,
        };
        if completion_sender.send(completion).is_err() {
            break;
        }
    }
}

fn run_completer(
    accounts_db: Weak<AccountsDb>,
    completion_receiver: Receiver<Completion>,
    done_sender: Sender<Pubkey>,
) {
    for Completion {
        pubkey,
        slot,
        account,
    } in completion_receiver
    {
        if let Some(account) = account {
            let Some(accounts_db) = accounts_db.upgrade() else {
                break;
            };
            if accounts_db.store_in_read_cache_if_newest(&pubkey, slot, &account) {
                accounts_db
                    .prefetcher
                    .stats
                    .num_stored
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        // reported even when nothing was stored, or the pubkey would never be prefetched again;
        // an error only means the dispatcher is gone
        let _ = done_sender.send(pubkey);
    }
}

impl AccountsDb {
    /// Loads `pubkeys` from storage off this thread and stores them in the read cache, so that
    /// a later load on the caller's critical path finds them there.
    ///
    /// `pubkeys` is in the caller's order, which is the order the accounts are about to be used.
    /// The caller's only cost is queueing it for the dispatcher; when that queue is full the
    /// batch is dropped. Pubkeys that either cache already holds are dropped: a write-cache
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

    /// Queues a read for each of `pubkeys` that no cache holds and no prefetch is loading.
    /// Returns false once the reader is gone.
    fn dispatch_prefetch(
        &self,
        pubkeys: Vec<Pubkey>,
        outstanding: &mut Outstanding,
        read_sender: &Sender<Read>,
    ) -> bool {
        let rooted_ancestors = Ancestors::default();
        let mut num_skipped_cached = 0;
        let mut num_skipped_outstanding = 0;
        let mut num_not_in_index = 0;
        let mut num_issued = 0;
        let mut reader_alive = true;
        for pubkey in pubkeys {
            if self.accounts_cache.contains_pubkey(&pubkey)
                || self.read_only_accounts_cache.contains_key(&pubkey)
            {
                num_skipped_cached += 1;
                continue;
            }
            if !outstanding.insert(pubkey) {
                num_skipped_outstanding += 1;
                continue;
            }
            let Some((slot, accessor)) =
                self.get_account_accessor_from_index(&pubkey, &rooted_ancestors)
            else {
                outstanding.remove(&pubkey);
                num_not_in_index += 1;
                continue;
            };
            let read = Read {
                pubkey,
                slot,
                accessor,
            };
            if read_sender.send(read).is_err() {
                reader_alive = false;
                break;
            }
            num_issued += 1;
        }

        let stats = &self.prefetcher.stats;
        stats
            .num_skipped_cached
            .fetch_add(num_skipped_cached, Ordering::Relaxed);
        stats
            .num_skipped_outstanding
            .fetch_add(num_skipped_outstanding, Ordering::Relaxed);
        stats
            .num_not_in_index
            .fetch_add(num_not_in_index, Ordering::Relaxed);
        stats.num_issued.fetch_add(num_issued, Ordering::Relaxed);
        reader_alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dispatch_prefetch_skips_outstanding() {
        let db = AccountsDb::default_for_tests();
        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::new(42, 1, AccountSharedData::default().owner());
        db.store_for_tests((0, &[(&pubkey, &account)][..]));
        db.add_root_and_flush_write_cache(0);

        // no reader: the reads just queue
        let (read_sender, read_receiver) = unbounded();
        let mut outstanding = Outstanding::default();

        assert!(db.dispatch_prefetch(vec![pubkey], &mut outstanding, &read_sender));
        assert_eq!(read_receiver.len(), 1);

        // still outstanding, so not read again
        assert!(db.dispatch_prefetch(vec![pubkey], &mut outstanding, &read_sender));
        assert_eq!(read_receiver.len(), 1);
        let stats = &db.prefetcher.stats;
        assert_eq!(stats.num_issued.load(Ordering::Relaxed), 1);
        assert_eq!(stats.num_skipped_outstanding.load(Ordering::Relaxed), 1);

        // once reported done, it is read again
        outstanding.remove(&pubkey);
        assert!(db.dispatch_prefetch(vec![pubkey], &mut outstanding, &read_sender));
        assert_eq!(read_receiver.len(), 2);
    }

    #[test]
    fn test_dispatch_prefetch_not_in_index() {
        let db = AccountsDb::default_for_tests();
        let pubkey = Pubkey::new_unique();
        let (read_sender, read_receiver) = unbounded();
        let mut outstanding = Outstanding::default();

        assert!(db.dispatch_prefetch(vec![pubkey], &mut outstanding, &read_sender));
        assert!(read_receiver.is_empty());
        // nothing is in flight, so nothing is left outstanding
        assert!(outstanding.is_empty());
        assert_eq!(
            db.prefetcher.stats.num_not_in_index.load(Ordering::Relaxed),
            1
        );
    }
}
