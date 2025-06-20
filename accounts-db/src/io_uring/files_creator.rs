use std::{
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
};

use io_uring::{opcode, squeue, types, IoUring};
use libc::{O_CREAT, O_NOATIME, O_NOFOLLOW, O_TRUNC, O_WRONLY};
use slab::Slab;

use agave_io_uring::{Completion, Ring, RingOp};
use smallvec::SmallVec;

use crate::io_uring::{
    memory::{IoFixedBuffer, LargeBuffer},
    IO_PRIO_BE_HIGHEST,
};

const DEFAULT_WRITE_SIZE: usize = 1024 * 1024;
#[allow(dead_code)]
const DEFAULT_BUFFER_SIZE: usize = 64 * DEFAULT_WRITE_SIZE;

const MAX_OPEN_FILES: usize = 1000;
const MAX_IOWQ_WORKERS: u32 = 4;
const CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT: Option<Duration> = Some(Duration::from_millis(10));

/// Multiple files creator with `io_uring` queue for open -> write -> close
/// operations.
pub struct FilesCreator<F: FnMut(PathBuf), B = LargeBuffer> {
    ring: Ring<FileCreatorState<F>, FileCreatorOp<F>>,
    #[allow(dead_code)]
    backing_buffer: B,
}

struct FileCreatorState<F> {
    files: Slab<PendingFile>,
    buffers: VecDeque<IoFixedBuffer>,
    /// Externally provided callback to be called on paths of files that were written
    wrote_callback: F,
    /// No more work will be submitted
    is_draining: bool,
    open_fds: usize,
    /// Total write length of submitted writes
    pending_writes_size: usize,
    /// Count of cases when more than half of buffers are free (files are written
    /// faster than submitted - consider less buffers or speeding up submission)
    stats_large_buf_headroom_count: u32,
    /// Count of cases when we run out of free buffers (files are not written fast
    /// enough - consider more buffers or tuning write bandwidth / patterns)
    stats_no_buf_count: u32,
    /// Minimum total length of outstanding writes when encountering no buf headroom
    stats_no_buf_min_pending_writes_size: usize,
}

impl<F> FileCreatorState<F> {
    fn new(buffers: VecDeque<IoFixedBuffer>, wrote_callback: F) -> Self {
        Self {
            files: Slab::with_capacity(MAX_OPEN_FILES),
            buffers,
            wrote_callback,
            is_draining: false,
            open_fds: 0,
            pending_writes_size: 0,
            stats_no_buf_count: 0,
            stats_large_buf_headroom_count: 0,
            stats_no_buf_min_pending_writes_size: usize::MAX,
        }
    }
}

impl<F: FnMut(PathBuf)> FilesCreator<F, LargeBuffer> {
    /// Create a new `FilesCreator` using `wrote_callback` to notify caller when
    /// file contents are already persisted.
    ///
    /// See [FilesCreator::with_buffer] for more information.
    #[allow(dead_code)]
    pub fn new(wrote_callback: F) -> io::Result<Self> {
        Self::with_capacity(DEFAULT_BUFFER_SIZE, wrote_callback)
    }

    /// Create a new `FilesCreator` using internally allocated buffer of specified
    /// `buf_size` and default write size.
    pub fn with_capacity(buf_size: usize, wrote_callback: F) -> io::Result<Self> {
        Self::with_buffer(
            LargeBuffer::new(buf_size),
            DEFAULT_WRITE_SIZE,
            wrote_callback,
        )
    }

    /// Access the inner `wrote_callback` function
    pub fn wrote_callback(&mut self) -> &mut F {
        &mut self.ring.context_mut().wrote_callback
    }

    /// Schedule creating a file at `path` with `mode` permissons and
    /// bytes read from `contents`.
    #[allow(unused)]
    pub fn create(&mut self, path: PathBuf, mode: u32, contents: impl Read) -> io::Result<()> {
        let file_key = self.open(path, mode, None)?;
        self.write_and_close(contents, file_key)?;
        Ok(())
    }

    /// Schedule creating a file at `at_dir` directory using `path.file_name()`
    /// (`at_dir` is assumed to be a parent directory of `path`, which allows absolute `path`
    /// to be used for sending into `F`) and `mode` permissons with bytes read from `contents`.
    #[allow(unused)]
    pub fn create_at(
        &mut self,
        path: PathBuf,
        at_dir: Arc<File>,
        mode: u32,
        contents: impl Read,
    ) -> io::Result<()> {
        let file_key = self.open(path, mode, Some(at_dir))?;
        self.write_and_close(contents, file_key)?;
        Ok(())
    }

    /// Waits for all operations to be completed
    pub fn drain(mut self) -> io::Result<()> {
        self.ring.context_mut().is_draining = true;
        let res = self.ring.drain();
        log::info!(
            "files creation stats - large buf headroom: {}, no buf count: {}, min pending writes at no buf: {}",
            self.ring.context().stats_large_buf_headroom_count,
            self.ring.context().stats_no_buf_count,
            self.ring.context().stats_no_buf_min_pending_writes_size,
        );
        res
    }

    /// Schedule opening file at `path` with `mode` permissons.
    ///
    /// Returns key that can be used for scheduling writes for it.
    fn open(&mut self, path: PathBuf, mode: u32, at_dir: Option<Arc<File>>) -> io::Result<u32> {
        let file = PendingFile::from_path(path);
        let path = Pin::new(file.zero_terminated_path_bytes(at_dir.is_some()));

        let file_key = self.wait_add_file(file)?;

        let op = FileCreatorOp::Open(OpenOp {
            at_dir,
            path,
            mode,
            file_key,
            _f: std::marker::PhantomData,
        });
        self.ring.push(op)?;

        Ok(file_key)
    }

    fn wait_add_file(&mut self, file: PendingFile) -> io::Result<u32> {
        while self.ring.context().files.len() >= self.ring.context().files.capacity() {
            log::warn!("too many open files");
            self.ring.process_completions()?;
            self.ring
                .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
        }
        let file_key = self.ring.context_mut().files.insert(file) as u32;
        Ok(file_key)
    }

    fn write_and_close(&mut self, mut src: impl Read, file_key: u32) -> io::Result<()> {
        let mut offset = 0;
        loop {
            let mut buf = self.wait_free_buf()?;

            let state = self.ring.context_mut();
            let file = &mut state.files[file_key as usize];

            let len = src.read(buf.as_mut())?;
            if len == 0 {
                file.eof = true;

                state.buffers.push_front(buf);
                if file.complete() {
                    let path = mem::replace(&mut file.path, PathBuf::new());
                    (state.wrote_callback)(path);
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
                    _f: std::marker::PhantomData,
                };
                state.pending_writes_size += len;
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
            self.ring
                .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
        }
    }
}

impl<B: AsMut<[u8]>, F: FnMut(PathBuf)> FilesCreator<F, B> {
    /// Create a new `FilesCreator` using provided `buffer` and `wrote_callback`
    /// to notify caller when file contents are already persisted.
    ///
    /// `buffer` is the internal buffer used for writing scheduled file contents.
    /// It must be at least `write_capacity` long. The creator will execute multiple
    /// `write_capacity` sized writes in parallel to empty the work queue of files to create.
    pub fn with_buffer(
        mut buffer: B,
        write_capacity: usize,
        wrote_callback: F,
    ) -> io::Result<Self> {
        // Let submission queue hold half of buffers before we explicitly syscall
        // to submit them for reading.
        let ring_qsize = (buffer.as_mut().len() / write_capacity / 2).max(1) as u32;
        let ring = IoUring::builder().setup_coop_taskrun().build(ring_qsize)?;
        ring.submitter()
            .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 0])?;
        Self::with_buffer_and_ring(ring, buffer, write_capacity, wrote_callback)
    }

    fn with_buffer_and_ring(
        ring: IoUring,
        mut backing_buffer: B,
        write_capacity: usize,
        wrote_callback: F,
    ) -> io::Result<Self> {
        let buffer = backing_buffer.as_mut();
        assert!(buffer.len() % write_capacity == 0);

        let buffers =
            IoFixedBuffer::register_and_chunk_buffer(&ring, buffer, write_capacity)?.collect();

        let ring = Ring::new(ring, FileCreatorState::new(buffers, wrote_callback));
        let fds = vec![-1; MAX_OPEN_FILES];
        unsafe { ring.register_files(&fds)? };

        Ok(Self {
            ring,
            backing_buffer,
        })
    }
}

struct OpenOp<F> {
    at_dir: Option<Arc<File>>,
    path: Pin<Vec<u8>>,
    mode: libc::mode_t,
    file_key: u32,
    _f: std::marker::PhantomData<F>,
}

impl<F> std::fmt::Debug for OpenOp<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOp")
            .field("path", &unsafe { CStr::from_ptr(self.path.as_ptr() as _) })
            .field("file_key", &self.file_key)
            .finish()
    }
}

impl<F: FnMut(PathBuf)> OpenOp<F> {
    fn entry(&mut self) -> squeue::Entry {
        let at_dir_fd = types::Fd(
            self.at_dir
                .as_ref()
                .map(AsRawFd::as_raw_fd)
                .unwrap_or(libc::AT_FDCWD),
        );
        opcode::OpenAt::new(at_dir_fd, self.path.as_ptr() as _)
            .flags(O_CREAT | O_TRUNC | O_NOFOLLOW | O_WRONLY | O_NOATIME)
            .mode(self.mode)
            .file_index(Some(
                types::DestinationSlot::try_from_slot_target(self.file_key as u32).unwrap(),
            ))
            .build()
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState<F>, FileCreatorOp<F>>,
        res: io::Result<i32>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        match res {
            Ok(_) => (),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Safety:
                // Self::path is guaranteed to be valid while it's referenced by pointer by the
                // corresponding squeue::Entry.
                log::warn!(
                    "retrying file open: {}",
                    CStr::from_bytes_until_nul(&self.path)
                        .unwrap()
                        .to_string_lossy()
                );
                ring.push(FileCreatorOp::Open(Self {
                    path: mem::replace(&mut self.path, Pin::new(vec![])),
                    at_dir: self.at_dir.clone(),
                    ..*self
                }));
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        let state = ring.context_mut();
        let file = &mut state.files[self.file_key as usize];
        file.completed_open = true;
        state.open_fds += 1;

        let mut backlog = mem::replace(&mut file.backlog, SmallVec::new());
        for (buf, offset, len) in backlog.drain(..) {
            let op = WriteOp {
                file_key: self.file_key,
                offset,
                buf,
                write_len: len,
                _f: std::marker::PhantomData,
            };
            ring.context_mut().pending_writes_size += len;
            ring.push(FileCreatorOp::Write(op));
        }

        Ok(())
    }
}

#[derive(Debug)]
struct CloseOp<F> {
    file_key: u32,
    _f: std::marker::PhantomData<F>,
}

impl<F: FnMut(PathBuf)> CloseOp<F> {
    fn new(file_key: u32) -> Self {
        Self {
            file_key,
            _f: std::marker::PhantomData,
        }
    }

    fn entry(&mut self) -> squeue::Entry {
        opcode::Close::new(types::Fixed(self.file_key)).build()
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState<F>, FileCreatorOp<F>>,
        res: io::Result<i32>,
    ) -> io::Result<()>
    where
        Self: Sized,
    {
        let _ = res?;

        let state = ring.context_mut();
        let _ = state.files.remove(self.file_key as usize);
        state.open_fds -= 1;
        if !state.is_draining && state.buffers.len() > state.buffers.capacity() / 2 {
            state.stats_large_buf_headroom_count += 1;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct WriteOp<F> {
    file_key: u32,
    offset: usize,
    buf: IoFixedBuffer,
    write_len: usize,
    _f: std::marker::PhantomData<F>,
}

impl<F: FnMut(PathBuf)> WriteOp<F> {
    fn entry(&mut self) -> squeue::Entry {
        let WriteOp {
            file_key,
            offset,
            buf,
            write_len,
            _f: _,
        } = self;

        opcode::WriteFixed::new(
            types::Fixed(*file_key),
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
        ring: &mut Completion<FileCreatorState<F>, FileCreatorOp<F>>,
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
            _f: _,
        } = self;
        let buf = mem::replace(buf, IoFixedBuffer::empty());

        assert_eq!(written, *write_len, "short write");

        let state = ring.context_mut();
        state.pending_writes_size -= *write_len;
        if state.buffers.is_empty() {
            state.stats_no_buf_min_pending_writes_size = state
                .stats_no_buf_min_pending_writes_size
                .min(state.pending_writes_size);
        }
        state.buffers.push_front(buf);

        let file = &mut state.files[*file_key as usize];
        file.writes_completed += 1;
        if file.complete() {
            let path = mem::replace(&mut file.path, PathBuf::new());
            (state.wrote_callback)(path);
            ring.push(FileCreatorOp::Close(CloseOp::new(*file_key)));
        }

        Ok(())
    }
}

#[derive(Debug)]
enum FileCreatorOp<F> {
    Open(OpenOp<F>),
    Close(CloseOp<F>),
    Write(WriteOp<F>),
}

impl<F: FnMut(PathBuf)> RingOp<FileCreatorState<F>> for FileCreatorOp<F> {
    fn entry(&mut self) -> squeue::Entry {
        match self {
            Self::Open(op) => op.entry(),
            Self::Close(op) => op.entry(),
            Self::Write(op) => op.entry(),
        }
    }

    fn complete(
        &mut self,
        ring: &mut Completion<FileCreatorState<F>, Self>,
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

#[derive(Debug)]
struct PendingFile {
    path: PathBuf,
    completed_open: bool,
    backlog: SmallVec<[(IoFixedBuffer, usize, usize); 8]>,
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
        let buf_ptr = path_bytes.as_mut_ptr() as *mut u8;
        let bytes = if only_filename {
            self.path.file_name().unwrap_or_default().as_bytes()
        } else {
            self.path.as_os_str().as_bytes()
        };
        assert!(bytes.len() <= path_bytes.capacity() - 1);
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

    fn complete(&self) -> bool {
        self.eof && self.writes_started == self.writes_completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{self, Cursor};

    fn read_file_to_string(path: &PathBuf) -> String {
        String::from_utf8(fs::read(path).expect("Failed to read file"))
            .expect("Failed to decode file contents")
    }

    #[test]
    fn test_create_and_drain_calls_callback_and_writes_contents() -> io::Result<()> {
        // Setup
        let temp_dir = tempfile::tempdir()?;
        let file_path = temp_dir.path().join("test.txt");
        let contents = "Hello, world!";
        let contents_cursor = Cursor::new(contents);

        // Shared state to capture callback invocations
        let mut callback_invoked_path: Option<PathBuf> = None;

        // Instantiate FilesCreator
        let mut creator = FilesCreator::new(|path: PathBuf| {
            callback_invoked_path.replace(path);
        })?;

        // Action
        creator.create(file_path.clone(), 0o644, contents_cursor)?;
        creator.drain()?;

        // Assertion
        assert_eq!(read_file_to_string(&file_path), contents);
        assert_eq!(callback_invoked_path, Some(file_path));

        Ok(())
    }

    #[test]
    fn test_multiple_file_creations() -> io::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let mut callback_counter = 0;

        let mut creator = FilesCreator::new(|path: PathBuf| {
            let contents = read_file_to_string(&path);
            assert!(contents.starts_with("File "));
            callback_counter += 1;
        })?;

        for i in 0..5 {
            let file_path = temp_dir.path().join(format!("file_{}.txt", i));
            let data = format!("File {}", i);
            creator.create(file_path, 0o600, Cursor::new(data))?;
        }
        creator.drain()?;

        assert_eq!(callback_counter, 5);
        Ok(())
    }
}
