use {
    crate::{
        file_io::FilesCreator,
        io_uring::{
            memory::{IoFixedBuffer, LargeBuffer},
            IO_PRIO_BE_HIGHEST,
        },
    },
    agave_io_uring::{Completion, Ring, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    libc::{O_CREAT, O_NOATIME, O_NOFOLLOW, O_TRUNC, O_WRONLY},
    slab::Slab,
    smallvec::SmallVec,
    std::{
        collections::VecDeque,
        ffi::CStr,
        fs::File,
        io::{self, Read},
        mem,
        os::{fd::AsRawFd, unix::ffi::OsStrExt as _},
        path::PathBuf,
        pin::Pin,
        ptr,
        sync::Arc,
        time::Duration,
    },
};

const DEFAULT_WRITE_SIZE: usize = 1024 * 1024;
#[allow(dead_code)]
const DEFAULT_BUFFER_SIZE: usize = 64 * DEFAULT_WRITE_SIZE;

const MAX_OPEN_FILES: usize = 1000;
const MAX_IOWQ_WORKERS: u32 = 4;
const CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT: Option<Duration> = Some(Duration::from_millis(10));

/// Multiple files creator with `io_uring` queue for open -> write -> close
/// operations.
pub struct IoUringFilesCreator<'a, B = LargeBuffer> {
    ring: Ring<FileCreatorState<'a>, FileCreatorOp>,
    #[allow(dead_code)]
    backing_buffer: B,
}

impl<'a> IoUringFilesCreator<'a, LargeBuffer> {
    /// Create a new `IoUringFilesCreator` using `wrote_callback` to notify caller when
    /// file contents are already persisted.
    ///
    /// See [IoUringFilesCreator::with_buffer] for more information.
    #[allow(dead_code)]
    pub fn new<F: FnMut(PathBuf) + 'a>(wrote_callback: F) -> Result<Self, (F, io::Error)> {
        Self::with_capacity(DEFAULT_BUFFER_SIZE, wrote_callback)
    }

    /// Create a new `IoUringFilesCreator` using internally allocated buffer of specified
    /// `buf_size` and default write size.
    pub fn with_capacity<F: FnMut(PathBuf) + 'a>(
        buf_size: usize,
        wrote_callback: F,
    ) -> Result<Self, (F, io::Error)> {
        Self::with_buffer(
            LargeBuffer::new(buf_size),
            DEFAULT_WRITE_SIZE,
            wrote_callback,
        )
    }
}

impl<'a, B: AsMut<[u8]>> IoUringFilesCreator<'a, B> {
    /// Create a new `IoUringFilesCreator` using provided `buffer` and `wrote_callback`
    /// to notify caller when file contents are already persisted.
    ///
    /// `buffer` is the internal buffer used for writing scheduled file contents.
    /// It must be at least `write_capacity` long. The creator will execute multiple
    /// `write_capacity` sized writes in parallel to empty the work queue of files to create.
    pub fn with_buffer<F: FnMut(PathBuf) + 'a>(
        mut buffer: B,
        write_capacity: usize,
        wrote_callback: F,
    ) -> Result<Self, (F, io::Error)> {
        // Let submission queue hold half of buffers before we explicitly syscall
        // to submit them for writing.
        let ring_qsize = (buffer.as_mut().len() / write_capacity / 2).max(1) as u32;
        match IoUring::builder()
            .setup_coop_taskrun()
            .build(ring_qsize)
            .and_then(|ring| {
                ring.submitter()
                    .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 0])?;
                Ok(ring)
            }) {
            Ok(ring) => Self::with_buffer_and_ring(ring, buffer, write_capacity, wrote_callback),
            Err(e) => Err((wrote_callback, e)),
        }
    }

    fn with_buffer_and_ring<F: FnMut(PathBuf) + 'a>(
        ring: IoUring,
        mut backing_buffer: B,
        write_capacity: usize,
        wrote_callback: F,
    ) -> Result<Self, (F, io::Error)> {
        let buffer = backing_buffer.as_mut();
        assert!(buffer.len() % write_capacity == 0);

        // Those are fixed file descriptor slots - OpenAt will active them by index
        let fds = vec![-1; MAX_OPEN_FILES];
        if let Err(error) = ring.submitter().register_files(&fds) {
            return Err((wrote_callback, error));
        }

        let state = match IoFixedBuffer::register_and_chunk_buffer(&ring, buffer, write_capacity) {
            Ok(buffers) => FileCreatorState::new(buffers.collect(), wrote_callback),
            Err(e) => return Err((wrote_callback, e)),
        };

        Ok(Self {
            ring: Ring::new(ring, state),
            backing_buffer,
        })
    }
}

impl<B> FilesCreator for IoUringFilesCreator<'_, B> {
    fn schedule_create(
        &mut self,
        path: PathBuf,
        mode: u32,
        contents: &mut dyn Read,
    ) -> io::Result<()> {
        let file_key = self.open(path, mode, None)?;
        self.write_and_close(contents, file_key)
    }

    fn schedule_create_with_dir(
        &mut self,
        path: PathBuf,
        mode: u32,
        parent_dir_handle: Arc<File>,
        contents: &mut dyn Read,
    ) -> io::Result<()> {
        let file_key = self.open(path, mode, Some(parent_dir_handle))?;
        self.write_and_close(contents, file_key)
    }

    fn on_written(&mut self, path: PathBuf) {
        (self.ring.context_mut().wrote_callback)(path)
    }

    fn drain(&mut self) -> io::Result<()> {
        let res = self.ring.drain();
        self.ring.context().log_stats();
        res
    }
}

impl<B> IoUringFilesCreator<'_, B> {
    /// Schedule opening file at `path` with `mode` permissons.
    ///
    /// Returns key that can be used for scheduling writes for it.
    fn open(
        &mut self,
        path: PathBuf,
        mode: u32,
        dir_handle: Option<Arc<File>>,
    ) -> io::Result<usize> {
        let file = PendingFile::from_path(path);
        let path_bytes = Pin::new(file.zero_terminated_path_bytes(dir_handle.is_some()));

        let file_key = self.wait_add_file(file)?;

        let op = FileCreatorOp::Open(OpenOp {
            dir_handle,
            path_bytes,
            mode,
            file_key,
        });
        self.ring.push(op)?;

        Ok(file_key)
    }

    fn wait_add_file(&mut self, file: PendingFile) -> io::Result<usize> {
        while self.ring.context().files.len() >= self.ring.context().files.capacity() {
            log::warn!("too many open files");
            self.ring.process_completions()?;
            self.ring
                .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
        }
        let file_key = self.ring.context_mut().files.insert(file);
        Ok(file_key)
    }

    fn write_and_close(&mut self, mut src: impl Read, file_key: usize) -> io::Result<()> {
        let mut offset = 0;
        loop {
            let mut buf = self.wait_free_buf()?;

            let state = self.ring.context_mut();
            let file = &mut state.files[file_key];

            let len = src.read(buf.as_mut())?;
            if len == 0 {
                file.eof = true;

                state.buffers.push_front(buf);
                if file.is_completed() {
                    (state.wrote_callback)(mem::take(&mut file.path));
                    self.ring
                        .push(FileCreatorOp::Close(CloseOp::new(file_key)))?;
                }
                break;
            }

            file.writes_started += 1;
            if file.completed_open {
                let op = WriteOp {
                    file_key,
                    offset,
                    buf,
                    write_len: len,
                };
                state.submitted_writes_size += len;
                self.ring.push(FileCreatorOp::Write(op))?;
            } else {
                file.backlog.push((buf, offset, len));
            }

            offset += len;
        }

        Ok(())
    }

    fn wait_free_buf(&mut self) -> io::Result<IoFixedBuffer> {
        loop {
            self.ring.process_completions()?;
            let state = self.ring.context_mut();
            if let Some(buf) = state.buffers.pop_front() {
                return Ok(buf);
            }
            state.stats_no_buf_count += 1;
            state.stats_no_buf_sum_submitted_write_sizes += state.submitted_writes_size;

            self.ring
                .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
        }
    }
}

struct FileCreatorState<'a> {
    files: Slab<PendingFile>,
    buffers: VecDeque<IoFixedBuffer>,
    /// Externally provided callback to be called on paths of files that were written
    wrote_callback: Box<dyn FnMut(PathBuf) + 'a>,
    open_fds: usize,
    /// Total write length of submitted writes
    submitted_writes_size: usize,
    /// Count of cases when more than half of buffers are free (files are written
    /// faster than submitted - consider less buffers or speeding up submission)
    stats_large_buf_headroom_count: u32,
    /// Count of cases when we run out of free buffers (files are not written fast
    /// enough - consider more buffers or tuning write bandwidth / patterns)
    stats_no_buf_count: u32,
    /// Sum of all outstanding write sizes at moments of encountering no free buf
    stats_no_buf_sum_submitted_write_sizes: usize,
}

impl<'a> FileCreatorState<'a> {
    fn new(buffers: VecDeque<IoFixedBuffer>, wrote_callback: impl FnMut(PathBuf) + 'a) -> Self {
        Self {
            files: Slab::with_capacity(MAX_OPEN_FILES),
            buffers,
            wrote_callback: Box::new(wrote_callback),
            open_fds: 0,
            submitted_writes_size: 0,
            stats_no_buf_count: 0,
            stats_large_buf_headroom_count: 0,
            stats_no_buf_sum_submitted_write_sizes: 0,
        }
    }

    /// Returns write backlog that needs to be submitted to IO ring
    fn mark_file_opened(&mut self, file_key: usize) -> SmallVec<[PendingWrite; 8]> {
        let file = &mut self.files[file_key];
        file.completed_open = true;
        self.open_fds += 1;
        if self.buffers.len() * 2 > self.buffers.capacity() {
            self.stats_large_buf_headroom_count += 1;
        }
        mem::take(&mut file.backlog)
    }

    /// Returns true if all of the writes are now done
    fn mark_write_completed(
        &mut self,
        file_key: usize,
        write_len: usize,
        buf: IoFixedBuffer,
    ) -> bool {
        self.submitted_writes_size -= write_len;
        self.buffers.push_front(buf);

        let file = &mut self.files[file_key];
        file.writes_completed += 1;
        if file.is_completed() {
            (self.wrote_callback)(mem::take(&mut file.path));
            return true;
        }
        false
    }

    fn mark_file_closed(&mut self, file_key: usize) {
        let _ = self.files.remove(file_key);
        self.open_fds -= 1;
    }

    fn log_stats(&self) {
        let avg_writes_at_no_buf = self
            .stats_no_buf_sum_submitted_write_sizes
            .checked_div(self.stats_no_buf_count as usize)
            .unwrap_or_default();
        log::info!(
            "files creation stats - large buf headroom: {}, no buf count: {}, avg pending writes at no buf: {}",
            self.stats_large_buf_headroom_count,
            self.stats_no_buf_count,
            avg_writes_at_no_buf,
        );
    }
}

#[derive(Debug)]
struct OpenOp {
    dir_handle: Option<Arc<File>>,
    path_bytes: Pin<Vec<u8>>,
    mode: libc::mode_t,
    file_key: usize,
}

impl OpenOp {
    fn entry(&mut self) -> squeue::Entry {
        let at_dir_fd = types::Fd(
            self.dir_handle
                .as_ref()
                .map(AsRawFd::as_raw_fd)
                .unwrap_or(libc::AT_FDCWD),
        );
        opcode::OpenAt::new(at_dir_fd, self.path_bytes.as_ptr() as _)
            .flags(O_CREAT | O_TRUNC | O_NOFOLLOW | O_WRONLY | O_NOATIME)
            .mode(self.mode)
            .file_index(Some(
                types::DestinationSlot::try_from_slot_target(self.file_key as u32).unwrap(),
            ))
            .build()
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState, FileCreatorOp>,
        res: io::Result<i32>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        match res {
            Ok(_) => (),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                log::warn!(
                    "retrying file open: {:?}",
                    CStr::from_bytes_until_nul(&self.path_bytes)
                );
                ring.push(FileCreatorOp::Open(Self {
                    path_bytes: mem::replace(&mut self.path_bytes, Pin::new(vec![])),
                    dir_handle: self.dir_handle.clone(),
                    ..*self
                }));
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        let backlog = ring.context_mut().mark_file_opened(self.file_key);
        for (buf, offset, len) in backlog {
            let op = WriteOp {
                file_key: self.file_key,
                offset,
                buf,
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
}

impl<'a> CloseOp {
    fn new(file_key: usize) -> Self {
        Self { file_key }
    }

    fn entry(&mut self) -> squeue::Entry {
        opcode::Close::new(types::Fixed(self.file_key as u32)).build()
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
        ring.context_mut().mark_file_closed(self.file_key);
        Ok(())
    }
}

#[derive(Debug)]
struct WriteOp {
    file_key: usize,
    offset: usize,
    buf: IoFixedBuffer,
    write_len: usize,
}

impl<'a> WriteOp {
    fn entry(&mut self) -> squeue::Entry {
        let WriteOp {
            file_key,
            offset,
            buf,
            write_len,
        } = self;

        opcode::WriteFixed::new(
            types::Fixed(*file_key as u32),
            buf.as_mut_ptr(),
            *write_len as u32,
            buf.io_buf_index(),
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
        let written = res? as usize;

        let WriteOp {
            file_key,
            offset: _,
            ref mut buf,
            write_len,
        } = self;

        assert_eq!(written, *write_len, "short write");

        let buf = mem::replace(buf, IoFixedBuffer::empty());
        if ring
            .context_mut()
            .mark_write_completed(*file_key, *write_len, buf)
        {
            ring.push(FileCreatorOp::Close(CloseOp::new(*file_key)));
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

type PendingWrite = (IoFixedBuffer, usize, usize);

#[derive(Debug)]
struct PendingFile {
    path: PathBuf,
    completed_open: bool,
    backlog: SmallVec<[PendingWrite; 8]>,
    eof: bool,
    writes_started: usize,
    writes_completed: usize,
}

impl PendingFile {
    fn from_path(path: PathBuf) -> Self {
        Self {
            path,
            completed_open: false,
            backlog: SmallVec::new(),
            writes_started: 0,
            writes_completed: 0,
            eof: false,
        }
    }

    fn zero_terminated_path_bytes(&self, only_filename: bool) -> Vec<u8> {
        let mut path_bytes = Vec::with_capacity(4096);
        let buf_ptr = path_bytes.as_mut_ptr();
        let bytes = if only_filename {
            self.path.file_name().unwrap_or_default().as_bytes()
        } else {
            self.path.as_os_str().as_bytes()
        };
        assert!(bytes.len() < path_bytes.capacity());
        // Safety:
        // We know that the buffer is large enough to hold the copy and the
        // pointers don't overlap.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, bytes.len());
            buf_ptr.add(bytes.len()).write(0);
            path_bytes.set_len(bytes.len() + 1);
        }
        path_bytes
    }

    fn is_completed(&self) -> bool {
        self.eof && self.writes_started == self.writes_completed
    }
}
