#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        file_io::FileCreator,
        io_uring::{
            memory::{FixedIoBuffer, LargeBuffer},
            sqpoll, IO_PRIO_BE_HIGHEST,
        },
        FileInfo,
    },
    agave_io_uring::{Completion, FixedSlab, Ring, RingOp},
    core::slice,
    io_uring::{opcode, squeue, types, IoUring},
    libc::{O_CREAT, O_NOATIME, O_NOFOLLOW, O_RDWR, O_TRUNC},
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
pub const DEFAULT_WRITE_SIZE: usize = 512 * 1024;

// 99.9% of accounts storage files are < 8MiB
type BacklogVec = SmallVec<[PendingWrite; 8 * 1024 * 1024 / DEFAULT_WRITE_SIZE]>;

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
const MAX_IOWQ_WORKERS: u32 = 4;

const CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT: Option<Duration> = Some(Duration::from_millis(10));

/// Multiple files creator with `io_uring` queue for open -> write -> close
/// operations.
pub struct IoUringFileCreator<'a, B = LargeBuffer> {
    ring: Ring<FileCreatorState<'a>, FileCreatorOp>,
    /// Owned buffer used (chunked into `FixedIoBuffer` items) across lifespan of `ring`
    /// (should get dropped last)
    _backing_buffer: B,
}

impl<'a> IoUringFileCreator<'a, LargeBuffer> {
    /// Create a new `IoUringFileCreator` using internally allocated buffer of specified
    /// `buf_size` and default write size.
    pub fn with_buffer_capacity<F: FnMut(FileInfo) -> Option<File> + 'a>(
        buf_size: usize,
        shared_sqpoll_fd: Option<BorrowedFd>,
        file_complete: F,
    ) -> io::Result<Self> {
        Self::with_buffer(
            LargeBuffer::new(buf_size),
            DEFAULT_WRITE_SIZE,
            shared_sqpoll_fd,
            file_complete,
        )
    }
}

impl<'a, B: AsMut<[u8]>> IoUringFileCreator<'a, B> {
    /// Create a new `IoUringFileCreator` using provided `buffer` and `file_complete`
    /// to notify caller with already written file.
    ///
    /// `buffer` is the internal buffer used for writing scheduled file contents.
    /// It must be at least `write_capacity` long. The creator will execute multiple
    /// `write_capacity` sized writes in parallel to empty the work queue of files to create.
    ///
    /// `file_complete` callback receives `FileInfo` with open file and its context, its return
    /// `Option<File>` allows passing file ownership back such that it's closed as no longer used.
    pub fn with_buffer<F: FnMut(FileInfo) -> Option<File> + 'a>(
        mut buffer: B,
        write_capacity: usize,
        shared_sqpoll_fd: Option<BorrowedFd>,
        file_complete: F,
    ) -> io::Result<Self> {
        // Let submission queue hold half of buffers before we explicitly syscall
        // to submit them for writing (lets kernel start processing before we run out of buffers,
        // but also amortizes number of `submit` syscalls made).
        let ring_qsize = (buffer.as_mut().len() / write_capacity / 2).max(1) as u32;

        let ring = sqpoll::io_uring_builder_with(shared_sqpoll_fd).build(ring_qsize)?;
        // Maximum number of spawned [bounded IO, unbounded IO] kernel threads, we don't expect
        // any unbounded work, but limit it to 1 just in case (0 leaves it unlimited).
        ring.submitter()
            .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 1])?;
        Self::with_buffer_and_ring(ring, buffer, write_capacity, file_complete)
    }

    fn with_buffer_and_ring<F: FnMut(FileInfo) -> Option<File> + 'a>(
        ring: IoUring,
        mut backing_buffer: B,
        write_capacity: usize,
        file_complete: F,
    ) -> io::Result<Self> {
        let buffer = backing_buffer.as_mut();
        // Take prefix of buffer that is aligned to write_capacity
        assert!(buffer.len() >= write_capacity);
        let write_aligned_buf_len = buffer.len() / write_capacity * write_capacity;
        let buffer = &mut buffer[..write_aligned_buf_len];

        // Safety: buffers contain unsafe pointers to `buffer`, but we make sure they are
        // dropped before `backing_buffer` is dropped.
        let buffers = unsafe { FixedIoBuffer::split_buffer_chunks(buffer, write_capacity) };
        let state = FileCreatorState::new(buffers.collect(), file_complete);
        let ring = Ring::new(ring, state);

        // Safety: kernel holds unsafe pointers to `buffer`, struct field declaration order
        // guarantees that the ring is destroyed before `_backing_buffer` is dropped.
        unsafe { FixedIoBuffer::register(buffer, &ring)? };

        Ok(Self {
            ring,
            _backing_buffer: backing_buffer,
        })
    }
}

impl<B> FileCreator for IoUringFileCreator<'_, B> {
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

    fn file_complete(&mut self, file: File, path: PathBuf, size: u64) {
        let file_info = FileInfo { file, path, size };
        (self.ring.context_mut().file_complete)(file_info);
    }

    fn drain(&mut self) -> io::Result<()> {
        let res = self.ring.drain();
        self.ring.context().log_stats();
        res
    }
}

impl<B> IoUringFileCreator<'_, B> {
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
        loop {
            let buf = self.wait_free_buf()?;

            let state = self.ring.context_mut();
            let file_state = state.files.get_mut(file_key).unwrap();

            // Safety: the buffer points to the valid memory backed by `self._backing_buffer`.
            // It's obtained from the queue of free buffers and is written to exclusively
            // here before being handled to the kernel or backlog in `file`.
            let mut_slice = unsafe { slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };
            let len = src.read(mut_slice)?;

            if len == 0 {
                file_state.size_on_eof = Some(offset as u64);

                state.buffers.push_front(buf);
                if let Some(file_info) = file_state.try_take_completed_file_info() {
                    match (state.file_complete)(file_info) {
                        Some(unconsumed_file) => self.ring.push(FileCreatorOp::Close(
                            CloseOp::new(file_key, unconsumed_file),
                        ))?,
                        None => state.mark_file_complete(file_key),
                    }
                }
                break;
            }

            file_state.writes_started += 1;
            if let Some(file) = &file_state.open_file {
                let op = WriteOp {
                    file_key,
                    fd: types::Fd(file.as_raw_fd()),
                    offset,
                    buf,
                    buf_offset: 0,
                    write_len: len,
                };
                state.submitted_writes_size += len;
                self.ring.push(FileCreatorOp::Write(op))?;
            } else {
                file_state.backlog.push((buf, offset, len));
            }

            offset += len;
        }

        Ok(())
    }

    fn wait_free_buf(&mut self) -> io::Result<FixedIoBuffer> {
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
    buffers: VecDeque<FixedIoBuffer>,
    /// Externally provided callback to be called on files that were written
    file_complete: Box<dyn FnMut(FileInfo) -> Option<File> + 'a>,
    num_owned_files: usize,
    /// Total write length of submitted writes
    submitted_writes_size: usize,
    stats: FileCreatorStats,
}

impl<'a> FileCreatorState<'a> {
    fn new(
        buffers: VecDeque<FixedIoBuffer>,
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
    fn mark_file_opened(&mut self, file_key: usize, fd: types::Fd) -> BacklogVec {
        let file = self.files.get_mut(file_key).unwrap();
        // Safety: we just received FD from io_uring open, so it's valid, track it in owned File
        file.open_file = Some(unsafe { File::from_raw_fd(fd.0) });
        self.num_owned_files += 1;
        if self.buffers.len() * 2 > self.buffers.capacity() {
            self.stats.large_buf_headroom_count += 1;
        }
        mem::take(&mut file.backlog)
    }

    /// Returns owned `File` if all of the writes are done, but the callback didn't claim it
    fn mark_write_completed(
        &mut self,
        file_key: usize,
        write_len: usize,
        buf: FixedIoBuffer,
    ) -> Option<File> {
        self.submitted_writes_size -= write_len;
        self.buffers.push_front(buf);

        let file_state = self.files.get_mut(file_key).unwrap();
        file_state.writes_completed += 1;
        if let Some(file_info) = file_state.try_take_completed_file_info() {
            return match (self.file_complete)(file_info) {
                unconsumed_file @ Some(_) => unconsumed_file,
                None => {
                    self.mark_file_complete(file_key);
                    None
                }
            };
        }
        None
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
}

impl OpenOp {
    fn entry(&mut self) -> squeue::Entry {
        let at_dir_fd = types::Fd(self.dir_handle.as_raw_fd());
        opcode::OpenAt::new(at_dir_fd, self.path_cstring.as_ptr() as _)
            .flags(O_CREAT | O_TRUNC | O_NOFOLLOW | O_RDWR | O_NOATIME)
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

        let backlog = ring.context_mut().mark_file_opened(self.file_key, fd);
        for (buf, offset, len) in backlog {
            let op = WriteOp {
                file_key: self.file_key,
                fd,
                offset,
                buf,
                buf_offset: 0,
                write_len: len,
            };
            ring.context_mut().submitted_writes_size += len;
            ring.push(FileCreatorOp::Write(op));
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
    offset: usize,
    buf: FixedIoBuffer,
    buf_offset: usize,
    write_len: usize,
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
        opcode::WriteFixed::new(
            *fd,
            unsafe { buf.as_mut_ptr().byte_add(*buf_offset) },
            *write_len as u32,
            buf.io_buf_index()
                .expect("should have a valid fixed buffer"),
        )
        .offset(*offset as u64)
        .ioprio(IO_PRIO_BE_HIGHEST)
        .build()
        .flags(squeue::Flags::ASYNC)
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
            Ok(res) => res as usize,
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

        let buf = mem::replace(buf, FixedIoBuffer::empty());
        let total_written = *buf_offset + written;

        if written < *write_len {
            log::warn!("short write ({written}/{}), file={}", *write_len, *file_key);
            ring.push(FileCreatorOp::Write(WriteOp {
                file_key: *file_key,
                fd: *fd,
                offset: *offset + written,
                buf,
                buf_offset: total_written,
                write_len: *write_len - written,
            }));
            return Ok(());
        }

        if let Some(unconsumed_file) =
            ring.context_mut()
                .mark_write_completed(*file_key, total_written, buf)
        {
            ring.push(FileCreatorOp::Close(CloseOp::new(
                *file_key,
                unconsumed_file,
            )));
        }

        Ok(())
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

type PendingWrite = (FixedIoBuffer, usize, usize);

#[derive(Debug)]
struct PendingFile {
    path: PathBuf,
    open_file: Option<File>,
    backlog: BacklogVec,
    size_on_eof: Option<u64>,
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
        }
    }

    fn path_cstring(&self) -> CString {
        let os_str = self.path.file_name().expect("path must contain filename");
        CString::new(os_str.as_encoded_bytes()).expect("path mustn't contain interior NULs")
    }

    fn try_take_completed_file_info(&mut self) -> Option<FileInfo> {
        if self.writes_started != self.writes_completed {
            return None;
        }
        let size = self.size_on_eof?;
        let file = self.open_file.take()?;
        let path = mem::take(&mut self.path);
        Some(FileInfo { file, size, path })
    }
}
