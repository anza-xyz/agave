use {
    super::{
        memory::{FixedIoBuffer, LargeBuffer},
        IO_PRIO_BE_HIGHEST,
    },
    crate::buffered_reader::FileBufRead,
    agave_io_uring::{Completion, Ring, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    std::{
        collections::VecDeque,
        fs::{File, OpenOptions},
        io::{self, BufRead, Read},
        marker::PhantomData,
        mem,
        ops::{Deref, DerefMut},
        os::{
            fd::{AsRawFd as _, RawFd},
            unix::fs::OpenOptionsExt,
        },
        path::Path,
        slice,
    },
};

// Based on transfers seen with `dd bs=SIZE` for NVME drives: values >=64KiB are fine,
// but peak at 1MiB. Also compare with particular NVME parameters, e.g.
// 32 pages (Maximum Data Transfer Size) * page size (MPSMIN = Memory Page Size) = 128KiB.
pub const DEFAULT_READ_SIZE: usize = 1024 * 1024;

/// One IO worker is able to provide approximately 0.5GiB/s read-ahead throughput from SSD
/// without causing application code to wait for data (subject to hardware and read size),
/// so this default should allow single threaded reader to provide >1GiB/s.
const DEFAULT_MAX_IOWQ_WORKERS: u32 = 2;

pub struct SequentialFileReaderBuilder {
    read_size: usize,
    max_iowq_workers: u32,
    ring_squeue_size: Option<u32>,
}

/// Utility for building `SequentialFileReader` with specified tuning options.
impl SequentialFileReaderBuilder {
    pub fn new() -> Self {
        Self {
            read_size: DEFAULT_READ_SIZE,
            max_iowq_workers: DEFAULT_MAX_IOWQ_WORKERS,
            ring_squeue_size: None,
        }
    }

    /// Override the default size of a single IO read operation
    ///
    /// This influences the concurrency, since buffer is divided into chunks of this size.
    pub fn read_size(mut self, read_size: usize) -> Self {
        self.read_size = read_size;
        self
    }

    /// Override the default number of kernel IO worker threads
    ///
    /// Kernel threads are relatively cheap and in case of read IO they don't lock or process data,
    /// so the actual CPU usage is low. They will regularly take a bit of cycles on random CPUs though.
    ///
    /// The default can be adjusted to:
    /// - higher value when using single reader with very cheap data processing code to increase read throughput
    /// - lower value when using multiple readers or expensive data processing code to reduce thrashing CPUs
    pub fn max_iowq_workers(mut self, workers: u32) -> Self {
        self.max_iowq_workers = workers;
        self
    }

    /// Override the ring submission queue size
    ///
    /// Since sqpoll is not used by the reader, this impacts how frequently we need to `submit`
    /// operations for kernel to start executing them. By default reader uses heuristic to
    /// amortize number of submissions / batch of pushed operations.
    ///
    /// This parameter can be adjusted to:
    /// - low values in order to achieve higher frequency of `submit` syscalls (kernel starts
    ///   executing operations sooner)
    /// - higher values to avoid necessity of `submit` when pushing operations (especially when
    ///   user manually triggers submission by calling `add_files_to_prefetch`)
    pub fn ring_squeue_size(mut self, ring_squeue_size: u32) -> Self {
        self.ring_squeue_size = Some(ring_squeue_size);
        self
    }

    /// Build a new `SequentialFileReader` with internally allocated buffer.
    ///
    /// Buffer will hold at least `buf_capacity` bytes (increased to `read_size` if it's lower).
    ///
    /// Initially the reader is idle and starts reading after the first file is added.
    /// The reader will execute multiple `read_size` sized reads in parallel to fill the buffer.
    pub fn build<'a>(
        self,
        buf_capacity: usize,
    ) -> io::Result<SequentialFileReader<'a, LargeBuffer>> {
        let buf_capacity = buf_capacity.max(self.read_size);
        let buffer = LargeBuffer::new(buf_capacity);
        self.build_with_buffer(buffer)
    }

    /// Build a new `SequentialFileReader` with a user-supplied buffer
    ///
    /// `buffer` is the internal buffer used for reading. It must be at least `read_size` long.
    ///
    /// Initially the reader is idle and starts reading after the first file is added.
    /// The reader will execute multiple `read_size` sized reads in parallel to fill the buffer.
    pub fn build_with_buffer<'a, B: AsMut<[u8]>>(
        self,
        mut buffer: B,
    ) -> io::Result<SequentialFileReader<'a, B>> {
        let buf_capacity = buffer.as_mut().len();

        // Let all buffers be submitted for reading at any time
        let max_inflight_ops = (buf_capacity / self.read_size) as u32;

        // Unless overriden, set the submission queue to fit half of ops to amortize `submit` calls
        // to just 2 per fully read buffer size.
        let ring_squeue_size = self
            .ring_squeue_size
            .unwrap_or((max_inflight_ops / 2).max(1));

        // agave io_uring uses cqsize to define state slab size, so cqsize == max inflight ops
        let ring = io_uring::IoUring::builder()
            .setup_cqsize(max_inflight_ops)
            .build(ring_squeue_size)?;

        // Maximum number of spawned [bounded IO, unbounded IO] kernel threads, we don't expect
        // any unbounded work, but limit it to 1 just in case (0 leaves it unlimited).
        ring.submitter()
            .register_iowq_max_workers(&mut [self.max_iowq_workers, 1])?;

        SequentialFileReader::with_buffer_and_ring(buffer, self.read_size, ring)
    }
}

/// Reader for non-seekable files.
///
/// Implements read-ahead using io_uring.
pub struct SequentialFileReader<'a, B> {
    // Note: inner state is tied to `_backing_buffer` - contains unsafe pointer references
    // to the buffer.
    inner: Ring<BuffersState, ReadOp>,
    state: SequentialFileReaderState,
    /// Owned buffer used (chunked into `FixedIoBuffer` items) across lifespan of `inner`
    /// (should get dropped last)
    _backing_buffer: B,
    _phantom: PhantomData<&'a ()>,
}

impl<B> SequentialFileReader<'_, B> {
    /// Create a new `SequentialFileReader` using a custom ring instance.
    fn with_buffer_and_ring(
        mut backing_buffer: B,
        read_capacity: usize,
        io_uring: IoUring,
    ) -> io::Result<Self>
    where
        B: AsMut<[u8]>,
    {
        let buffer = backing_buffer.as_mut();
        assert!(buffer.len() >= read_capacity, "buffer too small");
        let read_aligned_buf_len = buffer.len() / read_capacity * read_capacity;
        let buffer = &mut buffer[..read_aligned_buf_len];

        // Safety: buffers contain unsafe pointers to `buffer`, but we make sure they are
        // dropped before `backing_buffer` is dropped.
        let buffers = unsafe { FixedIoBuffer::split_buffer_chunks(buffer, read_capacity) }
            .map(ReadBufState::Uninit)
            .collect();
        let inner = Ring::new(io_uring, BuffersState(buffers));

        // Safety: kernel holds unsafe pointers to `buffer`, struct field declaration order
        // guarantees that the ring is destroyed before `_backing_buffer` is dropped.
        unsafe { FixedIoBuffer::register(buffer, &inner)? };

        Ok(Self {
            inner,
            state: SequentialFileReaderState::default(),
            _backing_buffer: backing_buffer,
            _phantom: PhantomData,
        })
    }

    /// Opens file under `path`, check its metadata to determine read limit and add it to the reader.
    ///
    /// See `add_owned_file_to_prefetch` for more details.
    pub fn add_path_to_prefetch(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOATIME)
            .open(path)?;
        let len = file.metadata()?.len() as usize;
        self.add_owned_file_to_prefetch(file, len)
    }

    /// Add `file` to read. Starts reading the file as soon as a buffer is available.
    ///
    /// The read finishes when EOF is reached or `read_limit` bytes are read.
    /// Multiple files can be added to the reader and they will be read-ahead in FIFO order.
    ///
    /// Reader takes ownership of the file and will drop it after it's done reading
    /// and `move_to_next_file` is called.
    pub fn add_owned_file_to_prefetch(&mut self, file: File, read_limit: usize) -> io::Result<()> {
        self.add_file_by_fd(file.as_raw_fd(), read_limit)?;
        self.state.owned_files.push_back(file);
        Ok(())
    }

    /// Caller must ensure that the file is not closed while the reader is using it.
    fn add_file_by_fd(&mut self, fd: RawFd, read_limit: usize) -> io::Result<()> {
        self.state.files.push_back(FileState::new(fd, read_limit));

        if self.state.all_buffers_used(self.inner.context()) {
            // Just added file to backlog, no reads can be started yet.
            return Ok(());
        }

        // There are free buffers, so we can start reading the new file.
        self.state.next_read_file_index =
            Some(self.state.next_read_file_index.map_or(0, |idx| idx + 1));

        // Start reading as many buffers as necessary for queued files.
        self.try_schedule_new_ops()
    }

    /// When reading multiple files, this method moves the reader to the next file.
    fn move_to_next_file(&mut self) -> io::Result<()> {
        let state = &mut self.state;

        let Some(removed_file) = state.files.pop_front() else {
            return Ok(());
        };

        // Always reset in-file and in-buffer state
        state.current_offset = 0;
        state.current_buf_pos = 0;
        state.current_buf_len = 0;
        state.left_to_consume = 0;

        // Reclaim current and all subsequent unread buffers of removed file as uninitialized.
        let sentinel_buf_index = state
            .files
            .front()
            .and_then(|f| f.start_buf_index)
            .unwrap_or(state.current_buf_index);
        let num_bufs = self.inner.context().len();
        loop {
            self.inner.process_completions()?;
            let current_buf = self.inner.context_mut().get_mut(state.current_buf_index);
            if current_buf.is_reading() {
                // Still no data, wait for more completions, but submit in case there are queued
                // entries in the submission queue.
                self.inner.submit()?;
                continue;
            }
            current_buf.transition_to_uninit();

            let next_buf_index = (state.current_buf_index + 1) % num_bufs;
            state.current_buf_index = next_buf_index;
            if sentinel_buf_index == next_buf_index {
                break;
            }
        }

        if state
            .owned_files
            .front()
            .is_some_and(|f| removed_file.is_same_file(f))
        {
            state.owned_files.pop_front();
        }

        if let Some(next_file_index) = state.next_read_file_index.as_mut() {
            // Since file was removed from front, all indices are shifted by one
            state.next_read_file_index = next_file_index.checked_sub(1);
            if state.next_read_file_index.is_none() {
                // The removed file was the current one being read
                if state.files.is_empty() {
                    // Reader is empty, reset buf indices to initial values
                    state.current_buf_index = 0;
                    state.next_read_buf_index = 0;
                } else {
                    // There are other files to read, start with the new first file
                    state.next_read_file_index = Some(0);
                }
            }
        }

        self.try_schedule_new_ops()
    }

    fn try_schedule_new_ops(&mut self) -> io::Result<()> {
        // Start reading as many buffers as necessary for queued files.
        while let Some(op) = self.state.next_read_op(self.inner.context_mut()) {
            self.inner.push(op)?;
        }
        Ok(())
    }

    fn wait_current_buf_full(&mut self) -> io::Result<bool> {
        if self.state.files.is_empty() {
            return Ok(false);
        }
        let num_bufs = self.inner.context().len();
        loop {
            self.inner.process_completions()?;

            let state = &mut self.state;
            let current_buf = &mut self.inner.context_mut().get_mut(state.current_buf_index);
            match current_buf {
                ReadBufState::Full { buf, eof_pos } => {
                    if state.current_buf_len == 0 {
                        state.current_buf_len = eof_pos.unwrap_or(buf.len());
                        if state.left_to_consume > 0 {
                            let consumed = state
                                .left_to_consume
                                .min((state.current_buf_len - state.current_buf_pos) as usize);
                            state.left_to_consume -= consumed;
                            state.current_buf_pos += consumed as u32;
                        }
                    }

                    // Note: we might have consumed whole buf from `left_to_consume`
                    if state.current_buf_pos < state.current_buf_len {
                        // We have some data available.
                        return Ok(true);
                    }

                    if eof_pos.is_some() {
                        // Last filled buf for the whole file (until `move_to_next_file` is called).
                        return Ok(false);
                    }
                    // We have finished consuming this buffer - reset its state.
                    current_buf.transition_to_uninit();

                    // Next `fill_buf` will use subsequent buffer.
                    state.move_to_next_buf(num_bufs);

                    // A buffer was freed, so try to queue up next read.
                    self.try_schedule_new_ops()?;
                }

                ReadBufState::Reading => {
                    // Still no data, wait for more completions, but submit in case there are queued
                    // entries in the submission queue.
                    self.inner.submit()?
                }

                ReadBufState::Uninit(_) => unreachable!("should be initialized"),
            }
            // Move to the next buffer and check again whether we have data.
        }
    }
}

impl<B: AsMut<[u8]>> Read for SequentialFileReader<'_, B> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let bytes_to_read = available.len().min(buf.len());
        if bytes_to_read == 0 {
            return Ok(0); // EOF or empty `buf`
        }
        buf[..bytes_to_read].copy_from_slice(&available[..bytes_to_read]);
        self.consume(bytes_to_read);
        Ok(bytes_to_read)
    }
}

impl<B: AsMut<[u8]>> BufRead for SequentialFileReader<'_, B> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.state.current_buf_pos == self.state.current_buf_len
            && !self.wait_current_buf_full()?
        {
            return Ok(&[]);
        }

        // At this point we must have data or be at EOF.
        let current_buf = self.inner.context().get_fast(self.state.current_buf_index);
        Ok(current_buf.slice(self.state.current_buf_pos, self.state.current_buf_len))
    }

    fn consume(&mut self, amt: usize) {
        self.state.consume(amt);
    }
}

impl<'a, B: AsMut<[u8]>> FileBufRead<'a> for SequentialFileReader<'a, B> {
    fn activate_file(&mut self, file: &'a File, read_limit: usize) -> io::Result<()> {
        while self
            .state
            .files
            .front()
            .is_some_and(|file_state| !file_state.is_same_file(file))
        {
            self.move_to_next_file()?;
        }
        if self.state.files.is_empty() {
            self.add_file_to_prefetch(file, read_limit)?;
        }
        Ok(())
    }

    fn get_file_offset(&self) -> usize {
        self.state.current_offset
    }

    fn add_file_to_prefetch(&mut self, file: &'a File, read_limit: usize) -> io::Result<()> {
        self.add_file_by_fd(file.as_raw_fd(), read_limit)
    }

    fn add_files_to_prefetch(
        &mut self,
        files: impl Iterator<Item = (&'a File, usize)>,
    ) -> io::Result<()> {
        for (file, read_limit) in files {
            self.add_file_by_fd(file.as_raw_fd(), read_limit)?;
        }
        self.inner.submit()
    }
}

/// Holds the state of all the buffers that may be submitted to the kernel for reading.
struct BuffersState(Box<[ReadBufState]>);

impl BuffersState {
    fn len(&self) -> u16 {
        self.0.len() as u16
    }

    fn get_mut(&mut self, index: u16) -> &mut ReadBufState {
        &mut self.0[index as usize]
    }

    #[inline]
    fn get_fast(&self, index: u16) -> &ReadBufState {
        debug_assert!(index < self.len());
        // Perf: skip bounds check for performance
        unsafe { self.0.get_unchecked(index as usize) }
    }
}

impl Deref for BuffersState {
    type Target = [ReadBufState];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BuffersState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Holds the state of the reader.
#[derive(Debug, Default)]
struct SequentialFileReaderState {
    // Note: file states operate on file descriptors of files that are assumed to be open,
    // which is guaranteed either by them being in `owned_files` or in case of file references
    // because they are added with reader's 'a lifetime.
    files: VecDeque<FileState>,

    /// Amount of bytes left to consume from next buffer(s) before returning them in `fill_buf()`.
    /// This is necessary to handle `consume()` calls beyond the current buffer.
    left_to_consume: usize,
    /// Index of `BuffersState` buffer to consume data from (0 if no file is being read)
    current_buf_index: u16,
    /// Position in buffer (pointed by `current_buf_index`) to consume data from
    current_buf_pos: u32,
    /// Cached length of the current buffer (0 until `wait_current_buf_full` initializes it)
    current_buf_len: u32,
    /// File offset of the next `fill_buf()` buffer available to consume
    current_offset: usize,

    /// Index in `self.files` of the file that is currently being read (can generate new read ops).
    next_read_file_index: Option<usize>,
    /// Index of `BuffersState` buffer that can be used for the next read operation.
    next_read_buf_index: u16,

    owned_files: VecDeque<File>,
}

impl SequentialFileReaderState {
    fn consume(&mut self, amt: usize) {
        if amt == 0 || self.files.is_empty() {
            return;
        }
        self.current_offset += amt;

        let unconsumed_buf_len = (self.current_buf_len - self.current_buf_pos) as usize;
        if amt <= unconsumed_buf_len {
            self.current_buf_pos += amt as u32;
        } else {
            self.current_buf_pos = self.current_buf_len;
            // Keep track of any bytes left to consume beyond current buffer, they will be
            // accounted for during next `wait_current_buf_full` call.
            self.left_to_consume += amt - unconsumed_buf_len;
        }
    }

    /// Return the next read operation for the reader.
    ///
    /// If all buffers are used or last file is already (being) read, returns `None`.
    ///
    /// Reads are issued for files added into the reader from first file at position 0
    /// to its limit / EOF and then for any subsequent files.
    fn next_read_op(&mut self, bufs: &mut [ReadBufState]) -> Option<ReadOp> {
        if self.all_buffers_used(bufs) {
            return None;
        }
        let num_bufs = bufs.len() as u16;
        loop {
            let read_file_index = self.next_read_file_index?;
            match self.files[read_file_index].next_read_op(self.next_read_buf_index, bufs) {
                Some(op) => {
                    self.next_read_buf_index = (self.next_read_buf_index + 1) % num_bufs;
                    return Some(op);
                }
                None => {
                    // Last read file reached its limit, try to move to the next file
                    if read_file_index < self.files.len() - 1 {
                        self.next_read_file_index = Some(read_file_index + 1);
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    fn move_to_next_buf(&mut self, num_bufs: u16) {
        self.current_buf_index = (self.current_buf_index + 1) % num_bufs;
        self.current_buf_pos = 0;
        // Buffer might still be reading, len will be intialized on first `wait_current_buf_full`
        self.current_buf_len = 0;
    }

    /// Returns `true` if there are no more buffers available for reading.
    fn all_buffers_used(&self, bufs: &[ReadBufState]) -> bool {
        bufs[self.next_read_buf_index as usize].is_used()
    }
}

/// Holds the state of a single file being read.
#[derive(Debug)]
struct FileState {
    raw_fd: RawFd,
    /// Limit file offset to read up to.
    read_limit: usize,
    /// Offset of the next byte to read from the file
    next_read_offset: usize,
    /// When the file is possible to read for the first time, it should be read from this buffer index
    start_buf_index: Option<u16>,
}

impl FileState {
    fn new(raw_fd: RawFd, read_limit: usize) -> Self {
        Self {
            raw_fd,
            read_limit,
            next_read_offset: 0,
            start_buf_index: None,
        }
    }

    fn is_same_file(&self, file: &File) -> bool {
        self.raw_fd == file.as_raw_fd()
    }

    /// Create a new read operation into the `bufs[index]` buffer and update file state.
    ///
    /// This is called whenever new reads can be scheduled (on added file or freed buffer).
    ///
    /// Returns `ReadOp` that will read
    /// [self.next_read_offset, self.next_read_offset + min(buf len, self.read_limit))
    /// from the file into `bufs[index]`. Once the read is complete the buffer changes into
    /// `Full` state and can be consumed.
    fn next_read_op(&mut self, index: u16, bufs: &mut [ReadBufState]) -> Option<ReadOp> {
        let Self {
            start_buf_index,
            raw_fd,
            next_read_offset: offset,
            read_limit,
        } = self;
        let left_to_read = read_limit.saturating_sub(*offset);
        if left_to_read == 0 {
            return None;
        }

        let buf = bufs[index as usize].transition_to_reading();

        let read_len = left_to_read.min(buf.len() as usize);
        let op = ReadOp {
            fd: types::Fd(*raw_fd),
            buf,
            buf_offset: 0,
            file_offset: *offset,
            read_len: read_len as u32, // it's trimmed by u32 buf.len() above
            is_last_read: left_to_read == read_len,
            reader_buf_index: index,
        };
        // Mark file state to start reading at `index` buffer
        if start_buf_index.is_none() {
            *start_buf_index = Some(index);
        }

        // We always advance by `read_len`. If we get a short read, we submit a new
        // read for the remaining data. See ReadOp::complete().
        *offset += read_len;

        Some(op)
    }
}

#[derive(Debug)]
enum ReadBufState {
    /// The buffer is pending submission to read queue (on initialization and
    /// in transition from `Full` to `Reading`).
    Uninit(FixedIoBuffer),
    /// The buffer is currently being read and there's a corresponding ReadOp in
    /// the ring.
    Reading,
    /// The buffer is filled and ready to be consumed.
    Full {
        buf: FixedIoBuffer,
        /// Position in `buf` at which 0-sized read (or requested read limit) was reached
        eof_pos: Option<u32>,
    },
}

impl ReadBufState {
    fn is_used(&self) -> bool {
        matches!(self, ReadBufState::Reading | ReadBufState::Full { .. })
    }

    fn is_reading(&self) -> bool {
        matches!(self, ReadBufState::Reading)
    }

    #[inline]
    fn slice(&self, start_pos: u32, end_pos: u32) -> &[u8] {
        match self {
            Self::Full { buf, eof_pos } => {
                debug_assert!(eof_pos.unwrap_or(buf.len()) >= end_pos);
                let limit = (end_pos - start_pos) as usize;
                unsafe { slice::from_raw_parts(buf.as_ptr().add(start_pos as usize), limit) }
            }
            Self::Uninit(_) | Self::Reading => {
                unreachable!("must call as_slice only on full buffer")
            }
        }
    }

    /// Marks the buffer as uninitialized (after it has been fully consumed).
    fn transition_to_uninit(&mut self) {
        match self {
            Self::Uninit(_) => (),
            Self::Reading => unreachable!("cannot reset a buffer that has pending read"),
            Self::Full { buf, .. } => {
                *self = ReadBufState::Uninit(mem::replace(buf, FixedIoBuffer::empty()));
            }
        }
    }

    /// Marks the buffer as being read and returns underlying buffer to pass to `ReadOp`.
    #[must_use]
    fn transition_to_reading(&mut self) -> FixedIoBuffer {
        let Self::Uninit(buf) = mem::replace(self, Self::Reading) else {
            unreachable!("buffer should be uninitialized")
        };
        buf
    }
}

#[derive(Debug)]
struct ReadOp {
    fd: types::Fd,
    buf: FixedIoBuffer,
    /// This is the offset inside the buffer. It's typically 0, but can be non-zero if a previous
    /// read returned less data than requested (because of EINTR or whatever) and we submitted a new
    /// read for the remaining data.
    buf_offset: u32,
    /// The offset in the file.
    file_offset: usize,
    /// The length of the read. This is typically `read_capacity` but can be less if a previous read
    /// returned less data than requested or `file_offset` is close to the end of read limit.
    read_len: u32,
    /// Indicates that after reading `read_len` we have reached configured read limit.
    is_last_read: bool,
    /// This is the index of the buffer in the reader's state. It's used to update the state once the
    /// read completes.
    reader_buf_index: u16,
}

impl RingOp<BuffersState> for ReadOp {
    fn entry(&mut self) -> squeue::Entry {
        let ReadOp {
            fd,
            ref mut buf,
            buf_offset,
            file_offset,
            read_len,
            is_last_read: _,
            reader_buf_index: _,
        } = *self;
        debug_assert!(buf_offset + read_len <= buf.len());
        opcode::ReadFixed::new(
            fd,
            // Safety: we assert that the buffer is large enough to hold the read.
            unsafe { buf.as_mut_ptr().byte_add(buf_offset as usize) },
            read_len,
            buf.io_buf_index()
                .expect("should have a valid fixed buffer"),
        )
        .offset(file_offset as u64)
        .ioprio(IO_PRIO_BE_HIGHEST)
        .build()
        .flags(squeue::Flags::ASYNC)
    }

    fn complete(
        &mut self,
        completion: &mut Completion<BuffersState, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let ReadOp {
            fd,
            ref mut buf,
            buf_offset,
            file_offset,
            read_len,
            is_last_read,
            reader_buf_index,
        } = *self;
        let buffers = completion.context_mut();

        let last_read_len = res? as u32;

        let total_read_len = buf_offset + last_read_len;
        let buf = mem::replace(buf, FixedIoBuffer::empty());

        if last_read_len > 0 && last_read_len < read_len {
            // Partial read, retry the op with updated offsets
            let op: ReadOp = ReadOp {
                fd,
                buf,
                buf_offset: total_read_len,
                file_offset: file_offset + last_read_len as usize,
                read_len: read_len - last_read_len,
                reader_buf_index,
                is_last_read,
            };
            // Safety:
            // The op points to a buffer which is guaranteed to be valid for the
            // lifetime of the operation
            completion.push(op);
        } else {
            buffers[reader_buf_index as usize] = ReadBufState::Full {
                buf,
                eof_pos: (last_read_len == 0 || is_last_read).then_some(total_read_len),
            };
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Seek, tempfile::NamedTempFile, test_case::test_case};

    fn read_as_vec(mut reader: impl Read) -> Vec<u8> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        buf
    }

    fn check_reading_file(file_size: usize, backing_buffer_size: usize, read_capacity: usize) {
        let pattern: Vec<u8> = (0..251).collect();

        // Create a temp file and write the pattern to it repeatedly
        let mut temp_file = NamedTempFile::new().unwrap();
        for _ in 0..file_size / pattern.len() {
            io::Write::write_all(&mut temp_file, &pattern).unwrap();
        }
        io::Write::write_all(&mut temp_file, &pattern[..file_size % pattern.len()]).unwrap();

        let buf = vec![0; backing_buffer_size];
        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(read_capacity)
            .build_with_buffer(buf)
            .unwrap();
        reader
            .add_owned_file_to_prefetch(File::open(temp_file.path()).unwrap(), usize::MAX)
            .unwrap();

        // Read contents from the reader and verify length
        let all_read_data = read_as_vec(&mut reader);
        assert_eq!(all_read_data.len(), file_size);
        assert_eq!(reader.get_file_offset(), file_size);

        // Verify the contents
        for (i, byte) in all_read_data.iter().enumerate() {
            assert_eq!(*byte, pattern[i % pattern.len()], "Mismatch - pos {}", i);
        }
    }

    /// Test with buffer larger than the whole file
    #[test]
    fn test_reading_small_file() {
        check_reading_file(2500, 4096, 1024);
        check_reading_file(2500, 4096, 2048);
        check_reading_file(2500, 4096, 4096);
    }

    /// Test with buffer smaller than the whole file
    #[test]
    fn test_reading_file_in_chunks() {
        check_reading_file(25_000, 16384, 1024);
        check_reading_file(25_000, 4096, 1024);
        check_reading_file(25_000, 4096, 2048);
        check_reading_file(25_000, 4096, 4096);
    }

    /// Test with buffer much smaller than the whole file
    #[test]
    fn test_reading_large_file() {
        check_reading_file(250_000, 32768, 1024);
        check_reading_file(250_000, 16384, 1024);
        check_reading_file(250_000, 4096, 1024);
        check_reading_file(250_000, 4096, 2048);
        check_reading_file(250_000, 4096, 4096);
    }

    #[test]
    fn test_add_file_ref() {
        let mut temp_file = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp_file, &[0xa, 0xb, 0xc]).unwrap();
        temp_file.rewind().unwrap();

        {
            let mut reader = SequentialFileReaderBuilder::new()
                .read_size(512)
                .build_with_buffer(vec![0; 1024])
                .unwrap();
            reader.add_file_to_prefetch(temp_file.as_file(), 3).unwrap();
            assert_eq!(read_as_vec(&mut reader), &[0xa, 0xb, 0xc]);
        }
        // Independently we can also read from the file directly
        assert_eq!(read_as_vec(&mut temp_file), &[0xa, 0xb, 0xc]);
    }

    #[test]
    fn test_multiple_unlimited_files() {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0xd, 0xe, 0xf, 0x10]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(512)
            .build_with_buffer(vec![0; 1024])
            .unwrap();

        let f1 = File::open(temp1.path()).unwrap();
        let f2 = File::open(temp2.path()).unwrap();
        reader.add_owned_file_to_prefetch(f1, usize::MAX).unwrap();
        reader.add_owned_file_to_prefetch(f2, usize::MAX).unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
        reader.move_to_next_file().unwrap();

        let f1 = File::open(temp1.path()).unwrap();
        reader.add_owned_file_to_prefetch(f1, usize::MAX).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);
    }

    #[test]
    fn test_multiple_limited_files() {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0xd, 0xe, 0xf, 0x10]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(512)
            .build_with_buffer(vec![0; 1024])
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 2).unwrap();
        reader.add_file_to_prefetch(temp2.as_file(), 3).unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 4).unwrap();
        reader.add_file_to_prefetch(temp2.as_file(), 5).unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);

        reader.add_file_to_prefetch(temp2.as_file(), 4).unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 2).unwrap();
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb]);
    }

    #[test_case(2048, 512)]
    #[test_case(256, 128)]
    #[test_case(32, 2)]
    fn test_multiple_limited_and_unlimited_files(buffer_size: usize, read_size: usize) {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc, 0xd]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0x10, 0x11, 0x12, 0x13, 0x14]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(read_size)
            .build(buffer_size)
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 2).unwrap();
        reader
            .add_file_to_prefetch(temp2.as_file(), usize::MAX)
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 3).unwrap();
        reader.add_file_to_prefetch(temp2.as_file(), 4).unwrap();
        reader
            .add_file_to_prefetch(temp1.as_file(), usize::MAX)
            .unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0x10, 0x11, 0x12, 0x13, 0x14]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0x10, 0x11, 0x12, 0x13]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc, 0xd]);
    }

    #[test_case(2048, 512)]
    #[test_case(256, 128)]
    #[test_case(256, 32)]
    fn test_multiple_medium_limited_files(buffer_size: usize, read_size: usize) {
        let pattern = (0..2000).map(|i| i as u8).collect::<Vec<_>>();
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &pattern).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &pattern[1000..]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(read_size)
            .build_with_buffer(vec![0; buffer_size])
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 1990).unwrap();
        reader.add_file_to_prefetch(temp2.as_file(), 1000).unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 2010).unwrap();

        assert_eq!(read_as_vec(&mut reader), &pattern[..1990]);

        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), &pattern[1000..]);

        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), pattern);
    }

    #[test]
    fn test_interleave_add_file_and_reads() {
        let pattern = (0..2000).map(|i| i as u8).collect::<Vec<_>>();
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &pattern).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &pattern[1000..]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(512)
            .build(1024)
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 1990).unwrap();
        assert_eq!(read_as_vec(&mut reader), &pattern[..1990]);
        reader.move_to_next_file().unwrap();

        for _ in 0..10 {
            reader.add_file_to_prefetch(temp2.as_file(), 1000).unwrap();
            assert_eq!(read_as_vec(&mut reader), &pattern[1000..]);
            reader.move_to_next_file().unwrap();

            reader.add_file_to_prefetch(temp1.as_file(), 2010).unwrap();
            assert_eq!(read_as_vec(&mut reader), &pattern[..2000]);
            reader.move_to_next_file().unwrap();
        }
        assert_eq!(read_as_vec(&mut reader), Vec::<u8>::new());

        for _ in 0..10 {
            reader.add_file_to_prefetch(temp2.as_file(), 1000).unwrap();
            reader.add_file_to_prefetch(temp1.as_file(), 2010).unwrap();

            assert_eq!(read_as_vec(&mut reader), &pattern[1000..]);
            reader.move_to_next_file().unwrap();
            assert_eq!(read_as_vec(&mut reader), &pattern[..2000]);
            reader.move_to_next_file().unwrap();
        }
        assert_eq!(read_as_vec(&mut reader), Vec::<u8>::new());
    }

    #[test]
    fn test_get_offset() {
        let pattern = (0..600).map(|i| i as u8).collect::<Vec<_>>();
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &pattern).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(512)
            .build_with_buffer(vec![0; 1024])
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 1990).unwrap();

        assert_eq!(512, reader.fill_buf().unwrap().len());
        assert_eq!(0, reader.get_file_offset());
        reader.consume(0);
        assert_eq!(0, reader.get_file_offset());

        reader.consume(40);
        assert_eq!(40, reader.get_file_offset());
        assert_eq!(472, reader.fill_buf().unwrap().len());

        reader.consume(472);
        assert_eq!(512, reader.get_file_offset());
        assert_eq!(88, reader.fill_buf().unwrap().len());
        reader.consume(0);
        assert_eq!(512, reader.get_file_offset());

        reader.consume(88);
        assert_eq!(600, reader.get_file_offset());
        assert_eq!(0, reader.fill_buf().unwrap().len());

        reader.move_to_next_file().unwrap();
        assert_eq!(0, reader.get_file_offset());
    }

    #[test]
    fn test_consume_skip_filled_buf_len() {
        let pattern = (0..6000).map(|i| i as u8).collect::<Vec<_>>();
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &pattern).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(512)
            .build_with_buffer(vec![0; 2048])
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 5990).unwrap();

        assert_eq!(reader.fill_buf().unwrap(), &pattern[..512]);
        assert_eq!(0, reader.get_file_offset());

        reader.consume(600);
        assert_eq!(600, reader.get_file_offset());
        assert_eq!(reader.fill_buf().unwrap(), &pattern[600..1024]);

        reader.consume(400);
        assert_eq!(1000, reader.get_file_offset());
        assert_eq!(reader.fill_buf().unwrap(), &pattern[1000..1024]);

        reader.consume(25);
        assert_eq!(reader.fill_buf().unwrap(), &pattern[1025..1536]);

        reader.consume(2000);
        assert_eq!(reader.fill_buf().unwrap(), &pattern[3025..3072]);
    }

    #[test]
    fn test_activate_file() {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0xd, 0xe, 0xf, 0x10]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_size(512)
            .build_with_buffer(vec![0; 1024])
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 3).unwrap();
        reader.add_file_to_prefetch(temp2.as_file(), 4).unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        reader.activate_file(temp2.as_file(), 4).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);

        reader.activate_file(temp1.as_file(), 4).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        let f1 = File::open(temp1.path()).unwrap();
        reader.add_owned_file_to_prefetch(f1, usize::MAX).unwrap();
        reader.move_to_next_file().unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        reader.activate_file(temp2.as_file(), 4).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
    }
}
