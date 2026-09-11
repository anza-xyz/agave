//! Off-thread log writer.
//!
//! Records are filtered and formatted on the calling thread, then handed to a bounded channel
//! that a single background thread drains in batches. Callers never issue the write syscall, so
//! a slow or stalled log sink cannot block a thread that holds a lock, and a burst of records
//! costs one `write` for the whole batch instead of one per record.
//!
//! The queue is bounded and drops records when full rather than blocking the caller. Dropping
//! log lines is preferable to stalling consensus, and drops are reported in-band so a gap in the
//! log is never silent.

use {
    crossbeam_channel::{Receiver, Sender, TrySendError, bounded},
    std::{
        fs::{File, OpenOptions},
        io::{self, Write},
        path::{Path, PathBuf},
        sync::{
            LazyLock,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::Duration,
    },
};

/// Queue depth, in records. At ~200 bytes per record this bounds queued log memory to a few MB.
const QUEUE_DEPTH: usize = 16 * 1024;
/// Records coalesced into a single `write` call.
const BATCH_RECORDS: usize = 1024;
/// Byte target for one batch, so a burst of large records does not balloon the batch buffer.
const BATCH_BYTES: usize = 256 * 1024;
/// Batch buffer capacity above which it is shrunk back down after writing.
const MAX_BATCH_CAPACITY: usize = 4 * BATCH_BYTES;
/// Cap on how long callers wait on the writer, so a wedged sink cannot hang a panicking thread.
const WRITER_TIMEOUT: Duration = Duration::from_secs(5);

/// Records dropped because the queue was full, awaiting report by the writer thread.
static DROPPED: AtomicU64 = AtomicU64::new(0);

static WRITER: LazyLock<Sender<Cmd>> = LazyLock::new(|| {
    let (sender, receiver) = bounded(QUEUE_DEPTH);
    thread::Builder::new()
        .name("solLogWriter".to_string())
        .spawn(move || run(receiver, Sink::Stderr))
        .expect("log writer thread must spawn");
    sender
});

enum Cmd {
    /// One fully formatted log record, newline included.
    Record(Vec<u8>),
    /// Write everything queued ahead of this, then start writing to `PathBuf` instead.
    Reopen(PathBuf),
    /// Write everything queued ahead of this, then signal the waiter.
    Flush(Sender<()>),
}

/// `env_logger` target that hands formatted records to the writer thread.
struct ChannelSink;

impl Write for ChannelSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match WRITER.try_send(Cmd::Record(buf.to_vec())) {
            Ok(()) => (),
            Err(TrySendError::Full(_)) => {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                // No writer thread left to drain the queue, so write inline rather than lose
                // the record.
                io::stderr().write_all(buf)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // `env_logger` flushes after every record. Waiting for the writer thread here would put
        // the caller back on the syscall path this indirection exists to avoid; the real barrier
        // is `writer::flush`, reached through `log::Log::flush`.
        Ok(())
    }
}

/// The `env_logger` target that routes records through the writer thread.
pub(crate) fn target() -> env_logger::Target {
    env_logger::Target::Pipe(Box::new(ChannelSink))
}

/// Blocks until every record queued before this call has been written.
///
/// Required before `process::exit`, which runs no destructors: without it the records explaining
/// a shutdown or a panic are still sitting in the queue.
pub(crate) fn flush() {
    // Capacity 1 so the writer's send never blocks, even if this call has already timed out.
    let (done, wait) = bounded(1);
    if WRITER
        .send_timeout(Cmd::Flush(done), WRITER_TIMEOUT)
        .is_err()
    {
        return;
    }
    let _ = wait.recv_timeout(WRITER_TIMEOUT);
}

/// Writes everything already queued to the current file, then switches to `logfile`.
pub(crate) fn reopen(logfile: &Path) {
    let _ = WRITER.send_timeout(Cmd::Reopen(logfile.to_path_buf()), WRITER_TIMEOUT);
}

enum Sink {
    Stderr,
    File(File),
}

impl Sink {
    fn open(logfile: &Path) -> Self {
        // Append mode so this handle and the `dup2`ed stderr can share the file: the kernel
        // makes each `write` to an `O_APPEND` fd atomic, so batches from either never tear.
        match OpenOptions::new().create(true).append(true).open(logfile) {
            Ok(file) => Sink::File(file),
            Err(err) => {
                eprintln!("Unable to open {}: {err}", logfile.display());
                Sink::Stderr
            }
        }
    }

    fn write_all(&mut self, buf: &[u8]) {
        let result = match self {
            Sink::Stderr => io::stderr().write_all(buf),
            Sink::File(file) => file.write_all(buf),
        };
        // A failed log write has nowhere left to report itself: logging the failure would go
        // back through this same sink.
        let _ = result;
    }
}

fn run(receiver: Receiver<Cmd>, mut sink: Sink) {
    let mut batch = Vec::with_capacity(BATCH_BYTES);
    // `recv` parks until there is work; everything already queued behind it joins the batch.
    while let Ok(first) = receiver.recv() {
        let mut next = Some(first);
        let mut records = 0usize;
        let mut barrier = None;

        while let Some(cmd) = next.take() {
            match cmd {
                Cmd::Record(record) => {
                    batch.extend_from_slice(&record);
                    records = records.saturating_add(1);
                }
                // `Reopen` and `Flush` are ordering barriers: every record queued ahead of them
                // has to reach the sink first, so stop batching and act once `batch` is written.
                other => {
                    barrier = Some(other);
                    break;
                }
            }
            if records < BATCH_RECORDS && batch.len() < BATCH_BYTES {
                next = receiver.try_recv().ok();
            }
        }

        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let _ = writeln!(
                batch,
                "{dropped} log records dropped, log writer fell behind"
            );
        }
        if !batch.is_empty() {
            sink.write_all(&batch);
        }
        batch.clear();
        if batch.capacity() > MAX_BATCH_CAPACITY {
            batch.shrink_to(BATCH_BYTES);
        }

        match barrier {
            Some(Cmd::Reopen(logfile)) => sink = Sink::open(&logfile),
            Some(Cmd::Flush(done)) => {
                let _ = done.send(());
            }
            Some(Cmd::Record(_)) | None => (),
        }
    }
}
