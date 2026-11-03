//! The prefetch thread on io_uring. Each pass of its loop does two things:
//! - dispatch: takes up to `MAX_PUBKEYS_PER_PASS` pubkeys from the queue and, for each one
//!   worth reading, looks up where the account is stored and pushes a read of it onto the ring
//! - handle completions: handles completed reads, waiting briefly for one only when the queue
//!   is empty
//!
//! Each account is read as a split file stores it: first its meta entry, then, if that does not
//! hold all its data, its data entry. The meta entry's size is unknown until it is parsed, so its
//! read is up to a page; nearly every account fits. The data entry is read straight into the
//! account's own buffer, on a kernel worker: even from the page cache, copying a large account
//! would stall every other read for milliseconds.
//!
//! Completions only read: each one leaves its account in `PrefetchState::finished`, and the
//! thread stores it in the read cache right after the ring call that completed it. So the db is
//! touched only by the thread's own loop, which holds it for one pass at a time.

use {
    super::{FinishedRead, PrefetchAccountsStatsSub, PrefetchesInFlight},
    crate::{
        accounts_db::{AccountsDb, LoadedAccountAccessor},
        accounts_file::{AccountDataRead, AccountFromMetaEntry, MetaEntryRead},
        append_vec::PAGE_SIZE,
        split_file::DATA_ENTRY_FIXED_SIZE,
    },
    agave_io_uring::{Completion, Ring, RingAccess, RingOp},
    io_uring::{IoUring, opcode, squeue, types},
    log::*,
    shaq::mpmc::{Consumer, ReadBatch},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        io::{self, IoSliceMut},
        mem,
        num::NonZeroUsize,
        os::fd::AsRawFd,
        slice,
        sync::{
            Weak,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    },
};

/// Accounts being read at once, each with one read at a time: its meta entry, then its data
/// entry. The ring's queues and op slab are sized to it. Bounds the prefetcher's share of the
/// device whatever pubkeys it is sent: the read rate is this over the read latency. Mainnet
/// peaked at 64, at an epoch boundary.
const MAX_IN_FLIGHT: u32 = 64;
/// Maximum number of kernel workers for reads of account data, and for reads that cannot complete
/// from the page cache and that the filesystem cannot wait on asynchronously. Used to offload long
/// copies from the page cache, and to avoid stalling the ring for milliseconds
const MAX_IOWQ_WORKERS: u32 = 4;
/// With reads in flight and the queue empty, how long to wait for one to complete before
/// checking the queue again: the most new pubkeys wait behind the reads already in flight, as a
/// caller cannot wake a wait on the ring
const COMPLETION_WAIT: Duration = Duration::from_micros(10);
/// Most pubkeys a pass takes from the queue: a run that pushes no reads, all cached or absent,
/// would otherwise keep completed reads waiting until it ends
const MAX_PUBKEYS_PER_PASS: NonZeroUsize = NonZeroUsize::new(128).unwrap();
/// With nothing in flight, how long to wait for pubkeys before checking whether to exit
const IDLE_WAIT: Duration = Duration::from_millis(100);

pub(super) struct RingPrefetcher {
    ring: Ring<PrefetchState, ReadOp>,
    /// upgraded for one pass at a time: held while idle, the db could never drop
    accounts_db: Weak<AccountsDb>,
    in_flight: PrefetchesInFlight,
}

impl RingPrefetcher {
    pub(super) fn new(accounts_db: Weak<AccountsDb>) -> io::Result<Self> {
        let ring = IoUring::builder()
            // the op slab is sized to the completion queue, so this is what caps reads in flight
            .setup_cqsize(MAX_IN_FLIGHT)
            .build(MAX_IN_FLIGHT)?;
        if let Err(err) = ring
            .submitter()
            .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 1])
        {
            warn!("prefetch kernel workers are not capped: {err}");
        }
        let state = PrefetchState {
            handling_submitted: false,
            stats_sub: PrefetchAccountsStatsSub::default(),
            meta_entry_buffers: (0..MAX_IN_FLIGHT)
                .map(|_| Vec::with_capacity(PAGE_SIZE))
                .collect(),
            finished: Vec::with_capacity(MAX_IN_FLIGHT as usize),
        };
        Ok(Self {
            ring: Ring::new(ring, state),
            accounts_db,
            in_flight: PrefetchesInFlight::default(),
        })
    }

    pub(super) fn run(mut self, consumer: &Consumer<Pubkey>, exit: &AtomicBool) {
        if let Err(err) = self.do_run(consumer, exit) {
            error!("prefetch thread stopped: {err}");
        }
        // the ring's ops point into buffers they own; let them finish before freeing them
        if let Err(err) = self.ring.drain() {
            error!("prefetch thread failed to drain: {err}");
        }
    }

    fn do_run(&mut self, consumer: &Consumer<Pubkey>, exit: &AtomicBool) -> io::Result<()> {
        loop {
            let pubkeys = if self.in_flight.is_empty() {
                // nothing in flight means no completions to handle, so block until pubkeys arrive
                let Some(pubkeys) = wait_for_pubkeys(consumer, exit) else {
                    return Ok(());
                };
                Some(pubkeys)
            } else {
                consumer.try_reserve_read_batch(MAX_PUBKEYS_PER_PASS)
            };
            let Some(accounts_db) = self.accounts_db.upgrade() else {
                return Ok(());
            };
            // fewer pubkeys than asked for means the queue is empty, so `handle_completions` may
            // wait on the ring
            let queue_empty = match pubkeys {
                Some(pubkeys) => {
                    let queue_empty = pubkeys.len() < MAX_PUBKEYS_PER_PASS.get();
                    // read in place, in the queue's slots, released when `pubkeys` drops
                    self.dispatch(&accounts_db, pubkeys.iter())?;
                    queue_empty
                }
                None => true,
            };
            self.handle_completions(&accounts_db, queue_empty)?;
            let stats_sub = mem::take(&mut self.ring.context_mut().stats_sub);
            accounts_db.prefetcher.stats.accumulate_sub_stats(stats_sub);
        }
    }

    /// For each pubkey worth reading, looks up where the account is stored and pushes a read of
    /// it
    fn dispatch<'a>(
        &mut self,
        accounts_db: &AccountsDb,
        pubkeys: impl ExactSizeIterator<Item = &'a Pubkey>,
    ) -> io::Result<()> {
        self.ring.context_mut().stats_sub.num_received += pubkeys.len() as u64;
        for pubkey in pubkeys {
            let stats_sub = &mut self.ring.context_mut().stats_sub;
            if let Some((slot, accessor)) = self.in_flight.start(accounts_db, *pubkey, stats_sub) {
                self.push_read(*pubkey, slot, accessor)?;
                // pushing and submitting handle completions too
                let state = self.ring.context_mut();
                self.in_flight
                    .finish(accounts_db, state.finished.drain(..), &mut state.stats_sub);
            }
        }
        Ok(())
    }

    /// Pushes the read of the meta entry of the account at `accessor`
    fn push_read(
        &mut self,
        pubkey: Pubkey,
        slot: Slot,
        accessor: LoadedAccountAccessor,
    ) -> io::Result<()> {
        let state = self.ring.context_mut();
        state.stats_sub.max_in_flight = state.stats_sub.max_in_flight.max(self.in_flight.len());
        let Some(meta_entry_read) = accessor.construct_meta_entry_read() else {
            // the offset is at or past the file's end: nothing to prefetch
            state.stats_sub.num_past_end += 1;
            state.finished.push((pubkey, slot, None));
            return Ok(());
        };
        let account = AccountRead {
            pubkey,
            slot,
            accessor,
        };
        // a buffer from the pool of one per read in flight
        let buffer = state
            .meta_entry_buffers
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(PAGE_SIZE));
        if let Err(err) = self
            .ring
            .push(ReadOp::meta_entry(account, meta_entry_read, buffer))
        {
            // the read is abandoned, so nothing else finishes it
            self.ring.context_mut().finished.push((pubkey, slot, None));
            return Err(err);
        }
        // `push` only queues the read; submit it now rather than after the whole batch is
        // dispatched, which can be too late for the account's use
        self.submit()
    }

    /// Handles completed reads, first waiting briefly for one if `wait`
    fn handle_completions(&mut self, accounts_db: &AccountsDb, wait: bool) -> io::Result<()> {
        if self.in_flight.is_empty() {
            return Ok(());
        }
        if wait {
            self.ring.submit_and_wait(1, Some(COMPLETION_WAIT))?;
        }
        self.ring.process_completions()?;
        // submit the data reads completions queued, rather than after the next dispatch
        self.submit()?;
        let state = self.ring.context_mut();
        self.in_flight
            .finish(accounts_db, state.finished.drain(..), &mut state.stats_sub);
        Ok(())
    }

    /// Submits queued reads, then handles the ones that completed during submission: their data
    /// was in the page cache, and handling them now finishes their accounts without waiting for
    /// the next `handle_completions`
    fn submit(&mut self) -> io::Result<()> {
        self.ring.submit()?;
        self.ring.context_mut().handling_submitted = true;
        let result = self.ring.process_completions();
        self.ring.context_mut().handling_submitted = false;
        result
    }
}

/// Blocks until pubkeys arrive, waking every `IDLE_WAIT` to check `exit`. `None` once it is set.
fn wait_for_pubkeys<'a>(
    consumer: &'a Consumer<Pubkey>,
    exit: &AtomicBool,
) -> Option<ReadBatch<'a, Pubkey>> {
    loop {
        if let Ok(pubkeys) = consumer.reserve_read_batch_timeout(MAX_PUBKEYS_PER_PASS, IDLE_WAIT) {
            return Some(pubkeys);
        }
        if exit.load(Ordering::Relaxed) {
            return None;
        }
    }
}

/// What completions need, held by the ring
struct PrefetchState {
    /// set while handling the completions posted during their own submission
    handling_submitted: bool,
    /// what this pass did so far
    stats_sub: PrefetchAccountsStatsSub,
    /// Buffers for meta entry reads, one per read in flight, allocated once. A meta entry read is
    /// up to a page, whatever the account's size: the meta entry says how much more to read, and
    /// that is read into a buffer of its own, which becomes the account's.
    meta_entry_buffers: Vec<Vec<u8>>,
    /// Reads done with, for the thread to finish: the account read, or `None` when there is
    /// none to store
    finished: Vec<FinishedRead>,
}

impl PrefetchState {
    /// Counts a completed read and how long it took
    fn count_read(&mut self, is_data_read: bool, read_us: u64) {
        let stats_sub = &mut self.stats_sub;
        if is_data_read {
            // always on a kernel worker
            stats_sub.num_data_reads += 1;
        } else if self.handling_submitted {
            stats_sub.num_page_cache_reads += 1;
        } else {
            stats_sub.num_disk_reads += 1;
        }
        stats_sub.read_us += read_us;
        stats_sub.max_read_us = stats_sub.max_read_us.max(read_us);
    }
}

/// The account a read is for
#[derive(Clone)]
struct AccountRead {
    pubkey: Pubkey,
    slot: Slot,
    /// where the account is stored: keeps the file open, and the account in it, until the read
    /// completes
    accessor: LoadedAccountAccessor,
}

/// One read of an account: its meta entry, then, if that does not hold all its data, its data
/// entry
struct ReadOp {
    account: AccountRead,
    pushed: Instant,
    read: Read,
}

enum Read {
    /// The account's meta entry, into a pooled buffer
    MetaEntry {
        meta_entry_read: MetaEntryRead,
        buffer: Vec<u8>,
    },
    /// The account's data entry: its header, if any, then its data, into its own buffer
    Data {
        data_read: AccountDataRead,
        /// its first `data_read.data_entry_header_len()` bytes hold the header
        header: [u8; DATA_ENTRY_FIXED_SIZE],
        data: Vec<u8>,
        /// set on submission: the header, then `data`
        iovecs: Option<[IoSliceMut<'static>; 2]>,
    },
}

impl ReadOp {
    fn meta_entry(account: AccountRead, meta_entry_read: MetaEntryRead, buffer: Vec<u8>) -> Self {
        Self {
            account,
            pushed: Instant::now(),
            read: Read::MetaEntry {
                meta_entry_read,
                buffer,
            },
        }
    }

    fn data(account: AccountRead, data_read: AccountDataRead) -> Self {
        let data = Vec::with_capacity(data_read.data_len());
        Self {
            account,
            pushed: Instant::now(),
            read: Read::Data {
                data_read,
                header: [0; DATA_ENTRY_FIXED_SIZE],
                data,
                iovecs: None,
            },
        }
    }
}

impl RingOp<PrefetchState> for ReadOp {
    fn entry(&mut self) -> squeue::Entry {
        let accounts = self.account.accessor.accounts_file();
        match &mut self.read {
            Read::MetaEntry {
                meta_entry_read,
                buffer,
            } => {
                let fd = types::Fd(accounts.meta_entry_file().as_raw_fd());
                opcode::Read::new(fd, buffer.as_mut_ptr(), meta_entry_read.size as u32)
                    .offset(meta_entry_read.offset)
                    .build()
            }
            Read::Data {
                data_read,
                header,
                data,
                iovecs,
            } => {
                let (data_file, _) = accounts
                    .account_data_file()
                    .expect("a data entry read is of the data file");
                let offset = data_read.offset();
                let data_len = data_read.data_len();
                let header_len = data_read.data_entry_header_len();
                // SAFETY: both point into this op, which the ring keeps in place until the read
                // completes
                let iovecs = iovecs.insert(unsafe {
                    [
                        IoSliceMut::new(slice::from_raw_parts_mut(header.as_mut_ptr(), header_len)),
                        IoSliceMut::new(slice::from_raw_parts_mut(data.as_mut_ptr(), data_len)),
                    ]
                });
                let fd = types::Fd(data_file.as_raw_fd());
                opcode::Readv::new(fd, iovecs.as_ptr().cast(), iovecs.len() as u32)
                    .offset(offset)
                    .build()
                    // copy on a worker, not on this thread during submit
                    .flags(squeue::Flags::ASYNC)
            }
        }
    }

    fn complete(
        &mut self,
        completion: &mut Completion<PrefetchState, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let is_data_read = matches!(self.read, Read::Data { .. });
        let read_us = self.pushed.elapsed().as_micros() as u64;
        completion.context_mut().count_read(is_data_read, read_us);
        let bytes_read = res.ok().map(|bytes_read| bytes_read as usize);
        let account = match &mut self.read {
            Read::MetaEntry {
                meta_entry_read,
                buffer,
            } => {
                let from_meta_entry = bytes_read.and_then(|bytes_read| {
                    // SAFETY: the read initialized the first `bytes_read` bytes of spare capacity
                    unsafe { buffer.set_len(bytes_read) };
                    self.account
                        .accessor
                        .accounts_file()
                        .account_from_meta_entry(meta_entry_read.offset, buffer)
                });
                // what the buffer held is parsed or copied out, so it goes back to the pool
                let mut buffer = mem::take(buffer);
                buffer.clear();
                completion.context_mut().meta_entry_buffers.push(buffer);
                match from_meta_entry {
                    Some(AccountFromMetaEntry::Complete(account)) => Some(account),
                    Some(AccountFromMetaEntry::Partial(data_read)) => {
                        // still in flight: the data entry read finishes the account
                        return completion.push(ReadOp::data(self.account.clone(), data_read));
                    }
                    None => None,
                }
            }
            Read::Data {
                data_read,
                header,
                data,
                ..
            } => {
                let data_len = data_read.data_len();
                let header_len = data_read.data_entry_header_len();
                // a failed or short read is left to a load on the critical path
                (bytes_read == Some(header_len + data_len))
                    .then(|| {
                        // SAFETY: the read filled the header, then `data_len` bytes of spare
                        // capacity
                        unsafe { data.set_len(data_len) };
                        data_read
                            .account_from_data_entry(&header[..header_len], mem::take(data))
                            .ok()
                    })
                    .flatten()
            }
        };
        completion
            .context_mut()
            .finished
            .push((self.account.pubkey, self.account.slot, account));
        Ok(())
    }
}
