//! The prefetch thread on io_uring. Each pass of its loop does two things:
//! - dispatch: takes incoming batches and, for each pubkey worth reading, looks up where the
//!   account is stored and pushes a read of it onto the ring
//! - reap: waits briefly for reads to complete; each completion stores its account in the read
//!   cache, or reads the account again, whole, when it continues past what was read
//!
//! An account's size is unknown until its header is read, so the first read is a page; nearly
//! every account fits.

use {
    super::{DispatchCounts, Outstanding, Read},
    crate::{
        account_storage_entry::AccountStorageEntry,
        accounts_db::{AccountsDb, LoadedAccountAccessor},
        accounts_file::AccountsFile,
        append_vec::{AccountFromPage, AppendVec, PAGE_SIZE, file_offset_from_logical},
    },
    agave_io_uring::{Completion, Ring, RingAccess, RingOp},
    crossbeam_channel::Receiver,
    io_uring::{IoUring, opcode, squeue, types},
    log::*,
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        io,
        os::fd::AsRawFd,
        sync::{Arc, Weak},
        time::Duration,
    },
};

/// Reads in flight at once; the ring's completion queue and op slab are sized to it
const MAX_IN_FLIGHT: u32 = 256;
/// Submission queue entries; a full queue is submitted to the kernel to make room
const SUBMISSION_QUEUE_SIZE: u32 = 64;
/// Kernel workers for reads that cannot complete from the page cache and that the filesystem
/// cannot wait on asynchronously
const MAX_IOWQ_WORKERS: u32 = 8;
/// While reads are in flight, how long to wait for one to complete before taking new batches
const COMPLETION_WAIT: Duration = Duration::from_micros(200);

pub(super) struct RingPrefetcher {
    ring: Ring<PrefetchState, ReadOp>,
}

/// What completions need, held by the ring so they can finish their reads themselves
struct PrefetchState {
    accounts_db: Weak<AccountsDb>,
    outstanding: Outstanding,
    /// accounts whose reads have been pushed and not finished
    num_in_flight: u32,
}

impl PrefetchState {
    /// Stores a finished read's account in the read cache and clears its pubkey from
    /// `outstanding`
    fn finish(&mut self, pubkey: Pubkey, slot: Slot, account: Option<AccountSharedData>) {
        self.num_in_flight -= 1;
        if let Some(accounts_db) = self.accounts_db.upgrade() {
            accounts_db.finish_prefetch(pubkey, slot, account, &mut self.outstanding);
        }
    }
}

/// A read of `len` bytes at the start of an account
struct ReadOp {
    pubkey: Pubkey,
    slot: Slot,
    /// keeps the file open, and the account in it, until the read completes
    storage: Arc<AccountStorageEntry>,
    offset: u64,
    /// read into its spare capacity, which holds `len` bytes
    buf: Vec<u8>,
    len: usize,
}

impl ReadOp {
    fn append_vec(&self) -> &AppendVec {
        match &self.storage.accounts {
            AccountsFile::AppendVec(append_vec) => append_vec,
            AccountsFile::Split(_) => unreachable!("split files are not read on the ring"),
        }
    }
}

impl RingOp<PrefetchState> for ReadOp {
    fn entry(&mut self) -> squeue::Entry {
        let fd = types::Fd(self.append_vec().file().as_raw_fd());
        opcode::Read::new(fd, self.buf.as_mut_ptr(), self.len as u32)
            .offset(self.offset)
            .build()
    }

    fn complete(
        &mut self,
        completion: &mut Completion<PrefetchState, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let account = match res {
            Err(_) => None,
            Ok(bytes_read) => {
                // SAFETY: the read initialized the first `bytes_read` bytes of spare capacity
                unsafe { self.buf.set_len(bytes_read as usize) };
                match AppendVec::account_from_page(self.offset, &self.buf) {
                    Some(AccountFromPage::Complete(account)) => Some(account),
                    Some(AccountFromPage::Partial {
                        data_offset,
                        data_len,
                        ..
                    }) => {
                        let len = (data_offset - self.offset) as usize + data_len;
                        if self.offset + len as u64 <= self.append_vec().len() as u64 {
                            // still in flight: the read of the whole account finishes it
                            return completion.push(ReadOp {
                                pubkey: self.pubkey,
                                slot: self.slot,
                                storage: Arc::clone(&self.storage),
                                offset: self.offset,
                                buf: Vec::with_capacity(len),
                                len,
                            });
                        }
                        // data past the end of the file
                        None
                    }
                    None => None,
                }
            }
        };
        completion
            .context_mut()
            .finish(self.pubkey, self.slot, account);
        Ok(())
    }
}

impl RingPrefetcher {
    pub(super) fn new(accounts_db: Weak<AccountsDb>) -> io::Result<Self> {
        let ring = IoUring::builder()
            .setup_cqsize(MAX_IN_FLIGHT)
            .build(SUBMISSION_QUEUE_SIZE)?;
        // [bounded, unbounded] workers; reads are bounded work
        ring.submitter()
            .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 1])?;
        let state = PrefetchState {
            accounts_db,
            outstanding: Outstanding::default(),
            num_in_flight: 0,
        };
        Ok(Self {
            ring: Ring::new(ring, state),
        })
    }

    pub(super) fn run(mut self, batch_receiver: Receiver<Vec<Pubkey>>) {
        if let Err(err) = self.do_run(&batch_receiver) {
            error!("prefetch thread stopped: {err}");
        }
        // the ring's ops point into buffers they own; let them finish before freeing them
        if let Err(err) = self.ring.drain() {
            error!("prefetch thread failed to drain: {err}");
        }
    }

    fn do_run(&mut self, batch_receiver: &Receiver<Vec<Pubkey>>) -> io::Result<()> {
        loop {
            // nothing in flight, so nothing to reap: block until there is work. With reads in
            // flight, `reap` waits on the ring instead, and new batches wait for it.
            let first_batch = if self.ring.context().num_in_flight == 0 {
                let Ok(pubkeys) = batch_receiver.recv() else {
                    return Ok(());
                };
                Some(pubkeys)
            } else {
                None
            };
            if !self.dispatch(first_batch, batch_receiver)? {
                return Ok(());
            }
            self.reap()?;
        }
    }

    /// Takes `first_batch` and any queued batches and, for each pubkey worth reading, looks up
    /// where the account is stored and pushes a read of it. Returns false once `accounts_db` is
    /// gone.
    fn dispatch(
        &mut self,
        first_batch: Option<Vec<Pubkey>>,
        batch_receiver: &Receiver<Vec<Pubkey>>,
    ) -> io::Result<bool> {
        let Some(accounts_db) = self.ring.context().accounts_db.upgrade() else {
            return Ok(false);
        };
        let mut counts = DispatchCounts::default();
        // a full ring makes `push` reap completions until there is room
        for pubkeys in first_batch.into_iter().chain(batch_receiver.try_iter()) {
            for pubkey in pubkeys {
                let outstanding = &mut self.ring.context_mut().outstanding;
                if let Some(read) = accounts_db.read_for_prefetch(pubkey, outstanding, &mut counts)
                {
                    self.push_read(read)?;
                }
            }
        }
        counts.add_to(&accounts_db.prefetcher.stats);
        Ok(true)
    }

    /// Waits briefly for reads to complete; each completion finishes its read or reads more
    fn reap(&mut self) -> io::Result<()> {
        if self.ring.context().num_in_flight == 0 {
            return Ok(());
        }
        self.ring.submit_and_wait(1, Some(COMPLETION_WAIT))?;
        self.ring.process_completions()?;
        // submit the whole-account reads completions queued, rather than after the next dispatch
        self.ring.submit()
    }

    fn push_read(&mut self, read: Read) -> io::Result<()> {
        let Read {
            pubkey,
            slot,
            mut accessor,
        } = read;
        self.ring.context_mut().num_in_flight += 1;
        let append_vec_read = match &accessor {
            LoadedAccountAccessor::Stored(Some((storage, offset))) => match &storage.accounts {
                AccountsFile::AppendVec(append_vec) => Some((
                    Arc::clone(storage),
                    file_offset_from_logical(*offset),
                    append_vec.len() as u64,
                )),
                AccountsFile::Split(_) => None,
            },
            LoadedAccountAccessor::Stored(None) => None,
        };
        let Some((storage, offset, file_len)) = append_vec_read else {
            // split files have no ring read yet, so they are read here, blocking
            let account =
                accessor.check_and_get_loaded_account_shared_data(None::<fn(_, &_, _) -> _>);
            self.ring.context_mut().finish(pubkey, slot, account);
            return Ok(());
        };
        let len = file_len.saturating_sub(offset).min(PAGE_SIZE as u64) as usize;
        if len == 0 {
            self.ring.context_mut().finish(pubkey, slot, None);
            return Ok(());
        }
        self.ring.push(ReadOp {
            pubkey,
            slot,
            storage,
            offset,
            buf: Vec::with_capacity(len),
            len,
        })?;
        // `push` only queues the read; submit it now rather than after the whole batch is
        // dispatched, which can be too late for the account's use
        self.ring.submit()
    }
}
