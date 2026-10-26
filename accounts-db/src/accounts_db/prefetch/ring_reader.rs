//! The prefetch reader on io_uring: one thread keeps many reads in flight.
//!
//! An account is read in up to two steps, as `AppendVec::get_account_shared_data` does: up to a
//! page at its offset, then, if its data continues past the page, the rest of the data.

use {
    super::{Completion, Read},
    crate::{
        account_storage_entry::AccountStorageEntry,
        accounts_db::LoadedAccountAccessor,
        accounts_file::AccountsFile,
        append_vec::{
            AccountFromPage, AccountMeta, AppendVec, PAGE_SIZE, file_offset_from_logical,
        },
    },
    agave_io_uring::{Completion as RingCompletion, Ring, RingAccess, RingOp},
    crossbeam_channel::{Receiver, Sender},
    io_uring::{IoUring, opcode, squeue, types},
    log::*,
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{io, mem, os::fd::AsRawFd, sync::Arc, time::Duration},
};

/// Reads in flight at once; the ring's completion queue and op slab are sized to it
const MAX_IN_FLIGHT: u32 = 256;
/// Submission queue entries; a full queue is submitted to the kernel to make room
const SUBMISSION_QUEUE_SIZE: u32 = 64;
/// Kernel workers for reads that cannot complete from the page cache and that the filesystem
/// cannot wait on asynchronously
const MAX_IOWQ_WORKERS: u32 = 8;
/// While reads are in flight, how long to wait for one to complete before taking new requests
const COMPLETION_WAIT: Duration = Duration::from_micros(200);

pub(super) struct RingReader {
    ring: Ring<ReaderState, ReadOp>,
}

struct ReaderState {
    completion_sender: Sender<Completion>,
    /// accounts whose reads have been pushed and not finished
    num_in_flight: u32,
}

impl ReaderState {
    fn finish(&mut self, pubkey: Pubkey, slot: Slot, account: Option<AccountSharedData>) {
        self.num_in_flight -= 1;
        // an error only means the completer is gone, and with it the db
        let _ = self.completion_sender.send(Completion {
            pubkey,
            slot,
            account,
        });
    }
}

struct ReadOp {
    pubkey: Pubkey,
    slot: Slot,
    /// keeps the file open, and the account in it, until the read completes
    storage: Arc<AccountStorageEntry>,
    step: ReadStep,
}

enum ReadStep {
    /// reading up to a page at the account's offset
    Page {
        offset: u64,
        buf: Box<[u8; PAGE_SIZE]>,
        len: usize,
    },
    /// reading the rest of the account's data
    Data {
        account_meta: AccountMeta,
        data_offset: u64,
        data: Vec<u8>,
        data_len: usize,
    },
}

impl ReadOp {
    fn append_vec(&self) -> &AppendVec {
        match &self.storage.accounts {
            AccountsFile::AppendVec(append_vec) => append_vec,
            AccountsFile::Split(_) => unreachable!("split files are not read on the ring"),
        }
    }
}

impl RingOp<ReaderState> for ReadOp {
    fn entry(&mut self) -> squeue::Entry {
        let fd = types::Fd(self.append_vec().file().as_raw_fd());
        match &mut self.step {
            ReadStep::Page { offset, buf, len } => {
                opcode::Read::new(fd, buf.as_mut_ptr(), *len as u32)
                    .offset(*offset)
                    .build()
            }
            // reads into `data`'s spare capacity, which holds `data_len` bytes
            ReadStep::Data {
                data_offset,
                data,
                data_len,
                ..
            } => opcode::Read::new(fd, data.as_mut_ptr(), *data_len as u32)
                .offset(*data_offset)
                .build(),
        }
    }

    fn complete(
        &mut self,
        completion: &mut RingCompletion<ReaderState, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let file_len = self.append_vec().len() as u64;
        let account = match (res, &mut self.step) {
            (Err(_), _) => None,
            (Ok(bytes_read), ReadStep::Page { offset, buf, .. }) => {
                match AppendVec::account_from_page(*offset, &buf[..bytes_read as usize]) {
                    Some(AccountFromPage::Complete(account)) => Some(account),
                    Some(AccountFromPage::Partial {
                        account_meta,
                        data_len,
                        data_offset,
                    }) if data_offset + data_len as u64 <= file_len => {
                        let op = ReadOp {
                            pubkey: self.pubkey,
                            slot: self.slot,
                            storage: Arc::clone(&self.storage),
                            step: ReadStep::Data {
                                account_meta,
                                data_offset,
                                data: Vec::with_capacity(data_len),
                                data_len,
                            },
                        };
                        // still in flight: the data read finishes it
                        return completion.push(op);
                    }
                    // unparsable, or data past the end of the file
                    _ => None,
                }
            }
            (
                Ok(bytes_read),
                ReadStep::Data {
                    account_meta,
                    data,
                    data_len,
                    ..
                },
            ) => (bytes_read as usize == *data_len).then(|| {
                // SAFETY: the read filled all `data_len` bytes of spare capacity
                unsafe { data.set_len(*data_len) };
                AppendVec::account_from_data(account_meta, mem::take(data))
            }),
        };
        completion
            .context_mut()
            .finish(self.pubkey, self.slot, account);
        Ok(())
    }
}

impl RingReader {
    pub(super) fn new(completion_sender: Sender<Completion>) -> io::Result<Self> {
        let ring = IoUring::builder()
            .setup_cqsize(MAX_IN_FLIGHT)
            .build(SUBMISSION_QUEUE_SIZE)?;
        // [bounded, unbounded] workers; reads are bounded work
        ring.submitter()
            .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 1])?;
        let state = ReaderState {
            completion_sender,
            num_in_flight: 0,
        };
        Ok(Self {
            ring: Ring::new(ring, state),
        })
    }

    pub(super) fn run(mut self, read_receiver: Receiver<Read>) {
        if let Err(err) = self.do_run(read_receiver) {
            error!("prefetch reader stopped: {err}");
        }
        // the ring's ops point into buffers they own; let them finish before freeing them
        if let Err(err) = self.ring.drain() {
            error!("prefetch reader failed to drain: {err}");
        }
    }

    fn do_run(&mut self, read_receiver: Receiver<Read>) -> io::Result<()> {
        loop {
            if self.ring.context().num_in_flight == 0 {
                // nothing in flight, so nothing to complete: wait for work
                let Ok(read) = read_receiver.recv() else {
                    return Ok(());
                };
                self.push_read(read)?;
            }
            while self.ring.context().num_in_flight < MAX_IN_FLIGHT
                && let Ok(read) = read_receiver.try_recv()
            {
                self.push_read(read)?;
            }
            self.ring.submit_and_wait(1, Some(COMPLETION_WAIT))?;
            self.ring.process_completions()?;
        }
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
            step: ReadStep::Page {
                offset,
                buf: Box::new([0; PAGE_SIZE]),
                len,
            },
        })
    }
}
