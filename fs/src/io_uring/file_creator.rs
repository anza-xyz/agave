#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        FileInfo, FileSize, IoSize,
        file_io::FileCreator,
        io_uring::{
            IO_PRIO_BE_HIGHEST,
            memory::{IoBufferChunk, PageAlignedMemory},
            sqpoll,
        },
    },
    agave_io_uring::{Completion, FixedSlab, Ring, RingAccess, RingOp},
    core::slice,
    io_uring::{IoUring, opcode, squeue, types},
    libc::{O_CREAT, O_DIRECT, O_NOATIME, O_NOFOLLOW, O_RDWR, O_TRUNC},
    smallvec::SmallVec,
    std::{
        collections::VecDeque,
        ffi::CString,
        fs::File,
        io::{self, Read},
        mem,
        os::fd::{AsRawFd, BorrowedFd, FromRawFd as _, IntoRawFd as _, RawFd},
        path::PathBuf,
        pin::Pin,
        sync::Arc,
        time::Duration,
    },
};

// Based on transfers seen with `dd bs=SIZE` for NVME drives: values >=64KiB are fine,
// but usually peak around 256KiB-1MiB. Also compare with particular NVME parameters, e.g.
// 32 pages (Maximum Data Transfer Size) * page size (MPSMIN = Memory Page Size) = 128KiB.
pub const DEFAULT_WRITE_SIZE: IoSize = 512 * 1024;

// Write size and file offset alignment for use with direct IO - all modern file systems
// effectively use this constant.
const DIRECT_IO_WRITE_LEN_ALIGNMENT: IoSize = 512;

// Status flags (updatable on file-descriptor) used as default upon file creation.
const DEFAULT_STATUS_FLAGS: libc::c_int = O_NOATIME;

// 99.9% of accounts storage files are < 8MiB
type BacklogVec = SmallVec<[PendingWrite; 8 * 1024 * 1024 / DEFAULT_WRITE_SIZE as usize]>;

// Sanity limit for slab size and number of concurrent operations, in practice with 0.5-1GiB
// buffer this is also close to the number of available buffers that small files will use up.
// Also, permitting too many open files results in many submitted open ops, which will contend
// on the directory inode lock.
const MAX_OPEN_FILES: usize = 512;

// We need a few threads to saturate the disk bandwidth, especially that we are writing lots
// of small files, so the number of ops / write size is high. We also need open ops and writes
// to run concurrently.
// We shouldn't use too many threads, as they will contend a lot to lock the directory inode
// (on open, since in accounts-db most files land in a single dir).
const DEFAULT_MAX_IOWQ_WORKERS: u32 = 4;

const CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT: Option<Duration> = Some(Duration::from_millis(10));

/// Utility for building [`IoUringFileCreator`] with specified tuning options.
pub struct IoUringFileCreatorBuilder<'sp> {
    write_capacity: IoSize,
    max_iowq_workers: u32,
    ring_squeue_size: Option<u32>,
    shared_sqpoll_fd: Option<BorrowedFd<'sp>>,
    /// Register buffer as fixed with the kernel
    register_buffer: bool,
    write_with_direct_io: bool,
}

impl<'sp> IoUringFileCreatorBuilder<'sp> {
    pub fn new() -> Self {
        Self {
            write_capacity: DEFAULT_WRITE_SIZE,
            max_iowq_workers: DEFAULT_MAX_IOWQ_WORKERS,
            ring_squeue_size: None,
            shared_sqpoll_fd: None,
            register_buffer: true,
            write_with_direct_io: false,
        }
    }

    /// Override the default size of a single IO write operation
    ///
    /// This influences the concurrency, since buffer is divided into chunks of this size.
    #[cfg(test)]
    pub fn write_capacity(mut self, write_capacity: IoSize) -> Self {
        self.write_capacity = write_capacity;
        self
    }

    /// Set whether to register buffer with `io_uring` for improved performance.
    ///
    /// Enabling requires available memlock ulimit to be higher than sizes of registered buffers.
    pub fn use_registered_buffers(mut self, register_buffers: bool) -> Self {
        self.register_buffer = register_buffers;
        self
    }

    /// Write files in direct-IO mode (disables kernel caching of written contents).
    ///
    /// Note that `File` passed in completion callback is still switched to non-direct IO mode.
    #[cfg(test)]
    pub fn write_with_direct_io(mut self, enable_direct_io: bool) -> Self {
        self.write_with_direct_io = enable_direct_io;
        self
    }

    /// Use (or remove) a shared kernel thread to drain submission queue for IO operations
    pub fn shared_sqpoll(mut self, shared_sqpoll_fd: Option<BorrowedFd<'sp>>) -> Self {
        self.shared_sqpoll_fd = shared_sqpoll_fd;
        self
    }

    /// Build a new [`IoUringFileCreator`] with internally allocated buffer and `file_complete`
    /// to notify caller with already written file.
    ///
    /// Buffer will hold at least `buf_capacity` bytes (increased to `write_capacity` if it's lower).
    ///
    /// The creator will execute multiple `write_capacity` sized writes in parallel to empty
    /// the work queue of files to create.
    ///
    /// `file_complete` callback receives [`FileInfo`] with open file and its context, its return
    /// `Option<File>` allows passing file ownership back such that it's closed as no longer used.
    pub fn build<'a, F>(
        self,
        buf_capacity: usize,
        file_complete: F,
    ) -> io::Result<IoUringFileCreator<'a>>
    where
        F: FnMut(FileInfo) -> Option<File> + 'a,
    {
        let buf_capacity = buf_capacity.max(self.write_capacity as usize);
        let buffer = PageAlignedMemory::new(buf_capacity)?;
        self.build_with_buffer(buffer, file_complete)
    }

    /// Build a new [`IoUringFileCreator`] using provided `buffer` and `file_complete`
    /// to notify caller with already written file.
    fn build_with_buffer<'a, F: FnMut(FileInfo) -> Option<File> + 'a>(
        self,
        mut buffer: PageAlignedMemory,
        file_complete: F,
    ) -> io::Result<IoUringFileCreator<'a>> {
        // Align buffer capacity to write capacity, so we always write equally sized chunks
        let buf_capacity =
            buffer.as_mut().len() / self.write_capacity as usize * self.write_capacity as usize;
        assert_ne!(buf_capacity, 0, "write size aligned buffer is too small");
        let buf_slice_mut = &mut buffer.as_mut()[..buf_capacity];

        // O_DIRECT writes have offset, size and buffer alignment restrictions, we guarantee
        // those by splitting buffer and writes by `write_capacity` and requiring its alignment.
        assert!(
            self.write_capacity
                .is_multiple_of(DIRECT_IO_WRITE_LEN_ALIGNMENT)
                || !self.write_with_direct_io,
            "write capacity ({}) must be multiple of {DIRECT_IO_WRITE_LEN_ALIGNMENT} for direct IO",
            self.write_capacity
        );

        // Safety: buffers contain unsafe pointers to `buffer`, but we make sure they are
        // dropped before `backing_buffer` is dropped.
        let buffers = unsafe {
            IoBufferChunk::split_buffer_chunks(
                buf_slice_mut,
                self.write_capacity,
                self.register_buffer,
            )
        };
        let state = FileCreatorState::new(buffers.collect(), file_complete);

        let io_uring = self.create_io_uring(buf_capacity)?;
        let ring = Ring::new(io_uring, state);

        if self.register_buffer {
            // Safety: kernel holds unsafe pointers to `buffer`, struct field declaration order
            // guarantees that the ring is destroyed before `_backing_buffer` is dropped.
            unsafe { IoBufferChunk::register(buf_slice_mut, &ring)? };
        }

        Ok(IoUringFileCreator {
            ring,
            write_with_direct_io: self.write_with_direct_io,
            _backing_buffer: buffer,
        })
    }

    fn create_io_uring(&self, buf_capacity: usize) -> io::Result<IoUring> {
        // Let submission queue hold half of buffers before we explicitly syscall
        // to submit them for writing (lets kernel start processing before we run out of buffers,
        // but also amortizes number of `submit` syscalls made).
        let ring_qsize = self
            .ring_squeue_size
            .unwrap_or((buf_capacity / self.write_capacity as usize / 2).max(1) as u32);

        let ring = sqpoll::io_uring_builder_with(self.shared_sqpoll_fd).build(ring_qsize)?;
        // Maximum number of spawned [bounded IO, unbounded IO] kernel threads, we don't expect
        // any unbounded work, but limit it to 1 just in case (0 leaves it unlimited).
        ring.submitter()
            .register_iowq_max_workers(&mut [self.max_iowq_workers, 1])?;
        Ok(ring)
    }
}

/// Multiple files creator with `io_uring` queue for open -> write -> close
/// operations.
pub struct IoUringFileCreator<'a> {
    ring: Ring<FileCreatorState<'a>, FileCreatorOp>,
    write_with_direct_io: bool,
    /// Owned buffer used (chunked into [`IoBufferChunk`] items) across lifespan of `ring`
    /// (should get dropped last)
    _backing_buffer: PageAlignedMemory,
}

impl FileCreator for IoUringFileCreator<'_> {
    fn schedule_create_at_dir(
        &mut self,
        path: PathBuf,
        mode: u32,
        parent_dir_handle: Arc<File>,
        contents: &mut dyn Read,
    ) -> io::Result<()> {
        let file_key = self.open(path, mode, parent_dir_handle)?;
        self.write_and_close(contents, file_key)
    }

    fn file_complete(&mut self, file: File, path: PathBuf, size: FileSize) {
        let file_info = FileInfo { file, path, size };
        (self.ring.context_mut().file_complete)(file_info);
    }

    fn drain(&mut self) -> io::Result<()> {
        let res = self.ring.drain();
        self.ring.context().log_stats();
        res
    }
}

impl IoUringFileCreator<'_> {
    /// Schedule opening file at `path` with `mode` permissions.
    ///
    /// Returns key that can be used for scheduling writes for it.
    fn open(&mut self, path: PathBuf, mode: u32, dir_handle: Arc<File>) -> io::Result<usize> {
        let file = PendingFile::from_path(path);
        let path_cstring = Pin::new(file.path_cstring());

        let file_key = self.wait_add_file(file)?;

        let op = FileCreatorOp::Open(OpenOp {
            dir_handle,
            path_cstring,
            mode,
            file_key,
            write_with_direct_io: self.write_with_direct_io,
        });
        self.ring.push(op)?;

        Ok(file_key)
    }

    fn wait_add_file(&mut self, file: PendingFile) -> io::Result<usize> {
        loop {
            self.ring.process_completions()?;
            if self.ring.context().files.len() < self.ring.context().files.capacity() {
                break;
            }
            self.ring
                .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
        }
        let file_key = self.ring.context_mut().files.insert(file);
        Ok(file_key)
    }

    fn write_and_close(&mut self, mut src: impl Read, file_key: usize) -> io::Result<()> {
        let mut offset = 0;
        let mut reached_eof = false;
        while !reached_eof {
            let buf = self.wait_free_buf()?;

            let state = self.ring.context_mut();
            let file_state = state.files.get_mut(file_key).unwrap();

            // Safety: the buffer points to the valid memory backed by `self._backing_buffer`.
            // It's obtained from the queue of free buffers and is written to exclusively
            // here before being handled to the kernel or backlog in `file`.
            let mut mut_slice =
                unsafe { slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len() as usize) };
            // Fill as much of the buffer as possible to avoid excess IO operations
            let mut write_len;
            loop {
                let len = src.read(mut_slice)?;
                if len == 0 {
                    reached_eof = true;
                    write_len = buf.len() - mut_slice.len() as IoSize;
                    file_state.size_on_eof = Some(write_len as FileSize + offset);
                    break;
                }
                if len == mut_slice.len() {
                    write_len = buf.len();
                    break;
                }
                mut_slice = &mut mut_slice[len..];
            }

            if self.write_with_direct_io && write_len < buf.len() {
                let align_truncated_write_len =
                    write_len / DIRECT_IO_WRITE_LEN_ALIGNMENT * DIRECT_IO_WRITE_LEN_ALIGNMENT;
                if align_truncated_write_len != write_len {
                    // Since file passed to `file_complete` will be switched to non-direct io mode, the last
                    // non-aligned write can be postponed until that is done
                    file_state.non_dio_eof_write = Some(FinalNonDirectIoWrite {
                        buf: None,
                        file_offset: offset + align_truncated_write_len as FileSize,
                        buf_offset: align_truncated_write_len,
                        write_len: write_len - align_truncated_write_len,
                    });
                    write_len = align_truncated_write_len;
                }
            }

            file_state.writes_started += 1;
            if let Some(file) = &file_state.open_file {
                let fd = types::Fd(file.as_raw_fd());
                if write_len == 0 {
                    // EOF was reached consuming `src` *and* there isn't any data left to write
                    // immediately (there might be some stored for non-direct IO mode). Treat it as
                    // a successful write, perform proper accounting and pass `buf` to the next
                    // write operation.
                    //
                    // Note: this logic might be happening with no pending writes (e.g. if `buf`
                    // was obtained just before `src` reached EOF), so it's possible that file
                    // completion will be triggered immediately.
                    return FileCreatorState::mark_write_completed(
                        &mut self.ring,
                        file_key,
                        fd,
                        true,
                        buf,
                    );
                }

                let op = WriteOp {
                    file_key,
                    fd,
                    offset,
                    buf,
                    buf_offset: 0,
                    write_len,
                };
                state.submitted_writes_size += write_len as usize;
                self.ring.push(FileCreatorOp::Write(op))?;
            } else {
                // Note: `write_len` might be 0 here, but it's handled on open op completion
                file_state.backlog.push((buf, offset, write_len));
            }

            offset += write_len as FileSize;
        }

        Ok(())
    }

    fn wait_free_buf(&mut self) -> io::Result<IoBufferChunk> {
        loop {
            self.ring.process_completions()?;
            let state = self.ring.context_mut();
            if let Some(buf) = state.buffers.pop_front() {
                return Ok(buf);
            }
            state.stats.no_buf_count += 1;
            state.stats.no_buf_sum_submitted_write_sizes += state.submitted_writes_size;

            self.ring
                .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
        }
    }
}

struct FileCreatorState<'a> {
    files: FixedSlab<PendingFile>,
    buffers: VecDeque<IoBufferChunk>,
    /// Externally provided callback to be called on files that were written
    file_complete: Box<dyn FnMut(FileInfo) -> Option<File> + 'a>,
    num_owned_files: usize,
    /// Total write length of submitted writes
    submitted_writes_size: usize,
    stats: FileCreatorStats,
}

impl<'a> FileCreatorState<'a> {
    fn new(
        buffers: VecDeque<IoBufferChunk>,
        file_complete: impl FnMut(FileInfo) -> Option<File> + 'a,
    ) -> Self {
        Self {
            files: FixedSlab::with_capacity(MAX_OPEN_FILES),
            buffers,
            file_complete: Box::new(file_complete),
            num_owned_files: 0,
            submitted_writes_size: 0,
            stats: FileCreatorStats::default(),
        }
    }

    /// Returns write backlog that needs to be submitted to IO ring
    fn mark_file_opened(&mut self, file_key: usize, fd: types::Fd, direct_io: bool) -> BacklogVec {
        let file = self.files.get_mut(file_key).unwrap();
        // Safety: we just received FD from io_uring open, so it's valid, track it in owned File
        file.open_file = Some(unsafe { File::from_raw_fd(fd.0) });
        file.file_uses_direct_io = direct_io;
        self.num_owned_files += 1;
        if self.buffers.len() * 2 > self.buffers.capacity() {
            self.stats.large_buf_headroom_count += 1;
        }
        mem::take(&mut file.backlog)
    }

    /// Calls `file_complete` callback with completed file info and optionally schedules close
    fn mark_write_completed(
        ring: &mut impl RingAccess<Context = Self, Operation = FileCreatorOp>,
        file_key: usize,
        fd: types::Fd,
        is_eof_write: bool,
        buf: IoBufferChunk,
    ) -> io::Result<()> {
        let this = ring.context_mut();

        let file_state = this.files.get_mut(file_key).unwrap();
        file_state.writes_completed += 1;

        match file_state.non_dio_eof_write.as_mut() {
            Some(eof_write) if is_eof_write => {
                // Buffer used for the EOF aligned write might still have remaining (non-aligned)
                // data to write. `non_dio_eof_write` has offsets into that buffer, store it. From
                // this point the last write is possible to be submitted.
                assert!(eof_write.buf.replace(buf).is_none())
            }
            // Otherwise return buffer to the pool
            _ => this.buffers.push_front(buf),
        }

        if file_state.required_writes_done() {
            // All aligned writes are done at this point, switch off direct-io before completing file
            file_state.ensure_direct_io_disabled(fd)?;

            // After disabling direct-IO, we may still have last write op to schedule.
            if let Some(op) = file_state.try_take_final_write_op(file_key, fd) {
                file_state.writes_started += 1;
                this.submitted_writes_size += op.write_len as usize;
                return ring.push(FileCreatorOp::Write(op));
            }

            if let Some(file_info) = file_state.take_completed_file_info() {
                match (this.file_complete)(file_info) {
                    Some(unconsumed_file) => ring.push(FileCreatorOp::Close(CloseOp::new(
                        file_key,
                        unconsumed_file,
                    )))?,
                    None => this.mark_file_complete(file_key),
                };
            }
        }
        Ok(())
    }

    fn mark_file_complete(&mut self, file_key: usize) {
        let _ = self.files.remove(file_key);
        self.num_owned_files -= 1;
    }

    fn log_stats(&self) {
        self.stats.log();
    }
}

#[derive(Debug, Default)]
struct FileCreatorStats {
    /// Count of cases when more than half of buffers are free (files are written
    /// faster than submitted - consider less buffers or speeding up submission)
    large_buf_headroom_count: u32,
    /// Count of cases when we run out of free buffers (files are not written fast
    /// enough - consider more buffers or tuning write bandwidth / patterns)
    no_buf_count: u32,
    /// Sum of all outstanding write sizes at moments of encountering no free buf
    no_buf_sum_submitted_write_sizes: usize,
}

impl FileCreatorStats {
    fn log(&self) {
        let avg_writes_at_no_buf = self
            .no_buf_sum_submitted_write_sizes
            .checked_div(self.no_buf_count as usize)
            .unwrap_or_default();
        log::info!(
            "files creation stats - large buf headroom: {}, no buf count: {}, avg pending writes \
             at no buf: {avg_writes_at_no_buf}",
            self.large_buf_headroom_count,
            self.no_buf_count,
        );
    }
}

#[derive(Debug)]
struct OpenOp {
    dir_handle: Arc<File>,
    path_cstring: Pin<CString>,
    mode: libc::mode_t,
    file_key: usize,
    write_with_direct_io: bool,
}

impl OpenOp {
    fn entry(&mut self) -> squeue::Entry {
        let at_dir_fd = types::Fd(self.dir_handle.as_raw_fd());
        let mut flags = O_CREAT | O_TRUNC | O_NOFOLLOW | O_RDWR | DEFAULT_STATUS_FLAGS;
        if self.write_with_direct_io {
            flags |= O_DIRECT;
        }
        opcode::OpenAt::new(at_dir_fd, self.path_cstring.as_ptr() as _)
            .flags(flags)
            .mode(self.mode)
            .build()
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState, FileCreatorOp>,
        res: io::Result<RawFd>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        let fd = types::Fd(res?);

        let backlog =
            ring.context_mut()
                .mark_file_opened(self.file_key, fd, self.write_with_direct_io);
        for (buf, offset, write_len) in backlog {
            if write_len == 0 {
                return FileCreatorState::mark_write_completed(ring, self.file_key, fd, true, buf);
            }
            let op = WriteOp {
                file_key: self.file_key,
                fd,
                offset,
                buf,
                buf_offset: 0,
                write_len,
            };
            ring.context_mut().submitted_writes_size += write_len as usize;
            ring.push(FileCreatorOp::Write(op))?;
        }

        Ok(())
    }
}

#[derive(Debug)]
struct CloseOp {
    file_key: usize,
    fd: types::Fd,
}

impl<'a> CloseOp {
    fn new(file_key: usize, file_to_close: File) -> Self {
        let fd = types::Fd(file_to_close.into_raw_fd());
        Self { file_key, fd }
    }

    fn entry(&mut self) -> squeue::Entry {
        opcode::Close::new(self.fd).build()
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState<'a>, FileCreatorOp>,
        res: io::Result<i32>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        let _ = res?;
        ring.context_mut().mark_file_complete(self.file_key);
        Ok(())
    }
}

#[derive(Debug)]
struct WriteOp {
    file_key: usize,
    fd: types::Fd,
    offset: FileSize,
    buf: IoBufferChunk,
    buf_offset: IoSize,
    write_len: IoSize,
}

impl<'a> WriteOp {
    fn entry(&mut self) -> squeue::Entry {
        let WriteOp {
            file_key: _,
            fd,
            offset,
            buf,
            buf_offset,
            write_len,
        } = self;

        // Safety: buf is owned by `WriteOp` during the operation handling by the kernel and
        // reclaimed after completion passed in a call to `mark_write_completed`.
        let buf_ptr = unsafe { buf.as_mut_ptr().byte_add(*buf_offset as usize) };

        let entry = match buf.io_buf_index() {
            Some(io_buf_index) => opcode::WriteFixed::new(*fd, buf_ptr, *write_len, io_buf_index)
                .offset(*offset)
                .ioprio(IO_PRIO_BE_HIGHEST)
                .build(),
            None => opcode::Write::new(*fd, buf_ptr, *write_len)
                .offset(*offset)
                .ioprio(IO_PRIO_BE_HIGHEST)
                .build(),
        };
        entry.flags(squeue::Flags::ASYNC)
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState<'a>, FileCreatorOp>,
        res: io::Result<i32>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        let written = match res {
            // Fail fast if no progress. FS should report an error (e.g. `StorageFull`) if the
            // condition isn't transient, but it's hard to verify without extra tracking.
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(res) => res as IoSize,
            Err(err) => return Err(err),
        };

        let WriteOp {
            file_key,
            fd,
            offset,
            buf,
            buf_offset,
            write_len,
        } = self;

        let buf = mem::replace(buf, IoBufferChunk::empty());

        ring.context_mut().submitted_writes_size -= written as usize;
        let total_buf_written = *buf_offset + written;
        if written < *write_len {
            log::warn!("short write ({written}/{}), file={}", *write_len, *file_key);
            return ring.push(FileCreatorOp::Write(WriteOp {
                file_key: *file_key,
                fd: *fd,
                offset: *offset + written as FileSize,
                buf,
                buf_offset: total_buf_written,
                write_len: *write_len - written,
            }));
        }

        FileCreatorState::mark_write_completed(
            ring,
            *file_key,
            *fd,
            total_buf_written < buf.len(),
            buf,
        )
    }
}

#[derive(Debug)]
enum FileCreatorOp {
    Open(OpenOp),
    Close(CloseOp),
    Write(WriteOp),
}

impl RingOp<FileCreatorState<'_>> for FileCreatorOp {
    fn entry(&mut self) -> squeue::Entry {
        match self {
            Self::Open(op) => op.entry(),
            Self::Close(op) => op.entry(),
            Self::Write(op) => op.entry(),
        }
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        match self {
            Self::Open(op) => op.complete(ring, res),
            Self::Close(op) => op.complete(ring, res),
            Self::Write(op) => op.complete(ring, res),
        }
    }
}

type PendingWrite = (IoBufferChunk, FileSize, IoSize);

#[derive(Debug)]
struct PendingFile {
    path: PathBuf,
    open_file: Option<File>,
    backlog: BacklogVec,
    size_on_eof: Option<FileSize>,
    file_uses_direct_io: bool,
    /// Extra write data populated for direct IO mode if there is non-aligned data at EOF
    non_dio_eof_write: Option<FinalNonDirectIoWrite>,
    writes_started: usize,
    writes_completed: usize,
}

impl PendingFile {
    fn from_path(path: PathBuf) -> Self {
        Self {
            path,
            open_file: None,
            backlog: SmallVec::new(),
            writes_started: 0,
            writes_completed: 0,
            size_on_eof: None,
            file_uses_direct_io: false,
            non_dio_eof_write: None,
        }
    }

    fn path_cstring(&self) -> CString {
        let os_str = self.path.file_name().expect("path must contain filename");
        CString::new(os_str.as_encoded_bytes()).expect("path mustn't contain interior NULs")
    }

    /// Check if all contents were read from source and scheduled writes are done
    ///
    /// Note: this returns `true` if current stage writes are done, there might still be
    /// last write to be scheduled using `non_dio_eof_write`
    fn required_writes_done(&self) -> bool {
        self.writes_started == self.writes_completed && self.source_fully_read()
    }

    /// Return true if all contents to be written for this file are already read
    ///
    /// When this condition is satisfied, all write ops are known (either added to ring,
    /// stored in `backlog` or in `non_dio_eof_write`)
    fn source_fully_read(&self) -> bool {
        self.size_on_eof.is_some()
    }

    /// Turn off direct IO if file is in this mode
    fn ensure_direct_io_disabled(&mut self, fd: types::Fd) -> io::Result<()> {
        if self.file_uses_direct_io {
            // F_SETFL only updates O_APPEND, O_ASYNC, O_DIRECT, O_NOATIME, out of which
            // creator only uses last two, so setting DEFAULT_STATUS_FLAGS disables O_DIRECT.
            // Safety: function operates on an open file descriptor
            let fcntl_res = unsafe { libc::fcntl(fd.0, libc::F_SETFL, DEFAULT_STATUS_FLAGS) };
            if fcntl_res == -1 {
                return Err(io::Error::last_os_error());
            }
            self.file_uses_direct_io = false;
        }
        Ok(())
    }

    /// Extract the final write to be submitted once file is switched off from direct IO mode
    fn try_take_final_write_op(&mut self, file_key: usize, fd: types::Fd) -> Option<WriteOp> {
        let FinalNonDirectIoWrite {
            buf,
            file_offset: offset,
            buf_offset,
            write_len,
        } = self.non_dio_eof_write.take()?;
        let buf = buf.expect("should contain buffer for last write");
        Some(WriteOp {
            file_key,
            fd,
            offset,
            buf,
            buf_offset,
            write_len,
        })
    }

    fn take_completed_file_info(&mut self) -> Option<FileInfo> {
        let size = self.size_on_eof.take().expect("content is fully read");
        let file = self.open_file.take().expect("called once on completion");
        let path = mem::take(&mut self.path);
        Some(FileInfo { file, size, path })
    }
}

/// Write at the end of file that needs to be made with switched off direct IO
///
/// The sequence of writes for direct-IO mode is:
/// * schedule all aligned writes
/// * if there is any non-aligned data to be written, store its offsets and size in LastNonDirectIoWrite
/// * whenever EOF write (doesn't use whole buffer) is done, move that buffer to LastNonDirectIoWrite
/// * after all scheduled aligned writes are done, disable direct-IO mode on FD
/// * schedule LastNonDirectIoWrite if there is any
/// * once all writes are done (no more last write to be scheduled), mark file completion
#[derive(Debug)]
struct FinalNonDirectIoWrite {
    /// The buffer is the same as the one used for the aligned write at the end of the file, value
    /// gets stored once that EOF aligned write is done (or buf acquired for 0-sized op is processed).
    buf: Option<IoBufferChunk>,
    file_offset: FileSize,
    buf_offset: IoSize,
    write_len: IoSize,
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Cursor, test_case::test_case};

    // Check several edge cases:
    // * creating empty file
    // * file content is a multiple of write size
    // * buffer holds single write size (1 internal buffer is used)
    // * negations and combinations of above
    #[test_case(0, 2 * 1024, 1024)]
    #[test_case(1024, 1024, 1024)]
    #[test_case(2 * 1024, 1024, 1024)]
    #[test_case(4 * 1024, 2 * 1024, 1024)]
    #[test_case(9 * 1024, 1024, 1024)]
    #[test_case(9 * 1024, 2 * 1024, 1024)]
    fn test_create_chunked_content(file_size: FileSize, buf_size: usize, write_size: IoSize) {
        let contents = vec![1u8; file_size as usize];
        let (contents_a, contents_b) = contents.split_at(file_size as usize / 3);
        // Content split such that creator will require >1 calls to `read` for filling single buf
        let mut contents = Cursor::new(contents_a).chain(contents_b);
        let mut created = false;

        let mut creator = IoUringFileCreatorBuilder::new()
            .write_capacity(write_size)
            .use_registered_buffers(false)
            .build(buf_size, |file_info| {
                assert_eq!(file_info.size, file_size);
                assert_eq!(file_info.file.metadata().unwrap().len(), file_info.size);
                created = true;
                Some(file_info.file)
            })
            .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let dir = Arc::new(File::open(temp_dir.path()).unwrap());
        let file_path = temp_dir.path().join("test.txt");
        creator
            .schedule_create_at_dir(file_path, 0o644, dir, &mut contents)
            .unwrap();
        creator.drain().unwrap();
        drop(creator);
        assert!(created);
    }

    #[test]
    fn test_non_registered_buffer_create() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut read_data = vec![];
        let callback = |mut fi: FileInfo| {
            fi.file.read_to_end(&mut read_data).unwrap();
            Some(fi.file)
        };

        let mut creator = IoUringFileCreatorBuilder::new()
            .write_capacity(4 * 1024)
            .use_registered_buffers(false)
            .build(16 * 1024, callback)
            .unwrap();

        let dir = Arc::new(File::open(temp_dir.path()).unwrap());
        let file_path = temp_dir.path().join("file.txt");
        let file_size = 64 * 1024;
        let data = (0..).take(file_size).map(|v| v as u8).collect::<Vec<_>>();
        creator
            .schedule_create_at_dir(file_path, 0o600, dir, &mut Cursor::new(&data))
            .unwrap();
        creator.drain().unwrap();

        drop(creator);
        assert_eq!(file_size, read_data.len());
        assert_eq!(data, read_data);
    }

    #[test_case(4 * DIRECT_IO_WRITE_LEN_ALIGNMENT, 4 * DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(4 * DIRECT_IO_WRITE_LEN_ALIGNMENT, 2 * DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(5 * DIRECT_IO_WRITE_LEN_ALIGNMENT, 2 * DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(1000 + 4 * DIRECT_IO_WRITE_LEN_ALIGNMENT, 2 * DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(1000 + 5 * DIRECT_IO_WRITE_LEN_ALIGNMENT, 2 * DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    fn test_direct_io_create(file_size: IoSize, buf_size: IoSize) {
        let mut read_data = Vec::with_capacity(file_size as usize);
        let callback = |mut fi: FileInfo| {
            fi.file.read_to_end(&mut read_data).unwrap();
            Some(fi.file)
        };

        let mut creator = IoUringFileCreatorBuilder::new()
            .write_capacity(8 * 1024)
            .write_with_direct_io(true)
            .build(buf_size as usize, callback)
            .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let dir = Arc::new(File::open(temp_dir.path()).unwrap());
        let file_path = temp_dir.path().join("file.txt");
        let data: Vec<_> = (0..).take(file_size as usize).map(|v| v as u8).collect();
        creator
            .schedule_create_at_dir(file_path, 0o600, dir, &mut Cursor::new(&data))
            .unwrap();
        creator.drain().unwrap();

        drop(creator);
        assert_eq!(file_size, read_data.len() as IoSize);
        assert_eq!(data, read_data);
    }
}
