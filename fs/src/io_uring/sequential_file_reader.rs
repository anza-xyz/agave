#![allow(clippy::arithmetic_side_effects)]

use {
    super::{
        IO_PRIO_BE_HIGHEST,
        memory::{IoBufferChunk, PageAlignedMemory},
    },
    crate::{FileSize, IoSize, buffered_reader::FileBufRead, io_uring::sqpoll},
    agave_io_uring::{Completion, Ring, RingAccess as _, RingOp},
    io_uring::{IoUring, opcode, squeue, types},
    std::{
        collections::VecDeque,
        fs::{File, OpenOptions},
        io::{self, BufRead, Read},
        marker::PhantomData,
        mem,
        ops::{Deref, DerefMut},
        os::{
            fd::{AsRawFd, BorrowedFd, RawFd},
            unix::fs::OpenOptionsExt,
        },
        path::Path,
        slice,
    },
};

// Based on transfers seen with `dd bs=SIZE` for NVME drives: values >=64KiB are fine,
// but peak at 1MiB. Also compare with particular NVME parameters, e.g.
// 32 pages (Maximum Data Transfer Size) * page size (MPSMIN = Memory Page Size) = 128KiB.
const DEFAULT_READ_SIZE: IoSize = 1024 * 1024;
// For large file we don't really use workers as few regularly submitted requests get handled
// within sqpoll thread. Allow some workers just in case, but limit them.
const DEFAULT_MAX_IOWQ_WORKERS: u32 = 2;
// This is conservative read size alignment for use with direct IO, some block devices may have
// relaxed requirements, but detecting it is not trivial.
const DIRECT_IO_READ_LEN_ALIGNMENT: IoSize = 4096;

/// Utility for building `SequentialFileReader` with specified tuning options.
pub struct SequentialFileReaderBuilder<'sp> {
    read_capacity: IoSize,
    max_iowq_workers: u32,
    ring_squeue_size: Option<u32>,
    shared_sqpoll_fd: Option<BorrowedFd<'sp>>,
    /// Register buffer as fixed with the kernel
    register_buffer: bool,
    /// Toggle option for opening files with the O_DIRECT flag
    use_direct_io: bool,
}

impl<'sp> SequentialFileReaderBuilder<'sp> {
    pub fn new() -> Self {
        Self {
            read_capacity: DEFAULT_READ_SIZE,
            max_iowq_workers: DEFAULT_MAX_IOWQ_WORKERS,
            ring_squeue_size: None,
            shared_sqpoll_fd: None,
            register_buffer: false,
            use_direct_io: false,
        }
    }

    /// Override the default size of a single IO read operation
    ///
    /// This influences the concurrency, since buffer is divided into chunks of this size.
    #[cfg(test)]
    pub fn read_capacity(mut self, read_capacity: IoSize) -> Self {
        self.read_capacity = read_capacity;
        self
    }

    /// Set whether to register buffer with `io_uring` for improved performance.
    ///
    /// Enabling requires available memlock ulimit to be higher than sizes of registered buffers.
    pub fn use_registered_buffers(mut self, register_buffers: bool) -> Self {
        self.register_buffer = register_buffers;
        self
    }

    /// Read files in direct-IO mode (disables kernel caching of read contents).
    ///
    /// Enabling requires the filesystem to support directio and `read_capacity`
    /// to be a multiple of 4096.
    pub fn use_direct_io(mut self, use_direct_io: bool) -> Self {
        self.use_direct_io = use_direct_io;
        self
    }

    /// Use (or remove) a shared kernel thread to drain submission queue for IO operations
    pub fn shared_sqpoll(mut self, shared_sqpoll_fd: Option<BorrowedFd<'sp>>) -> Self {
        self.shared_sqpoll_fd = shared_sqpoll_fd;
        self
    }

    /// Build a new `SequentialFileReader` with internally allocated buffer.
    ///
    /// Buffer will hold at least `buf_capacity` bytes (increased to `read_capacity` if it's lower).
    ///
    /// Initially the reader is idle and starts reading after `set_file` is called. It will then execute
    /// multiple `read_capacity` sized reads in parallel to fill the buffer.
    pub fn build<'a>(self, buf_capacity: usize) -> io::Result<SequentialFileReader<'a>> {
        let buf_capacity = buf_capacity.max(self.read_capacity as usize);
        let buffer = PageAlignedMemory::new(buf_capacity)?;
        self.build_with_buffer(buffer)
    }

    /// Build a new `SequentialFileReader` with a user-supplied buffer
    ///
    /// `buffer` is the internal buffer used for reading. It must be at least `read_capacity` long.
    ///
    /// Initially the reader is idle and starts reading after the first file is added.
    /// The reader will execute multiple `read_capacity` sized reads in parallel to fill the buffer.
    fn build_with_buffer<'a>(
        self,
        mut buffer: PageAlignedMemory,
    ) -> io::Result<SequentialFileReader<'a>> {
        // Align buffer capacity to read capacity, so we always read equally sized chunks
        let buf_capacity =
            buffer.as_mut().len() / self.read_capacity as usize * self.read_capacity as usize;
        assert_ne!(buf_capacity, 0, "read size aligned buffer is too small");
        let buf_slice_mut = &mut buffer.as_mut()[..buf_capacity];

        // Safety: buffers contain unsafe pointers to `buffer`, but we make sure they are
        // dropped before `backing_buffer` in `SequentialFileReader` is dropped.
        let buffers = unsafe {
            IoBufferChunk::split_buffer_chunks(
                buf_slice_mut,
                self.read_capacity,
                self.register_buffer,
            )
        }
        .map(ReadBufState::Uninit)
        .collect();

        let buffers_state = BuffersState(buffers);

        let io_uring = self.create_io_uring(buf_capacity)?;
        let ring = Ring::new(io_uring, buffers_state);

        if self.register_buffer {
            // Safety: kernel holds unsafe pointers to `buffer`, struct field declaration order
            // guarantees that the ring is destroyed before `_backing_buffer` is dropped.
            unsafe { IoBufferChunk::register(buf_slice_mut, &ring)? };
        }

        if self.use_direct_io {
            // O_DIRECT reads have size and alignment restrictions and must be into a sub-buffer of
            // some multiple of the fs block size (see https://man7.org/linux/man-pages/man2/open.2.html#NOTES).
            assert!(
                self.read_capacity
                    .is_multiple_of(DIRECT_IO_READ_LEN_ALIGNMENT),
                "read size is not aligned for direct IO({} is not a multiple of \
                 {DIRECT_IO_READ_LEN_ALIGNMENT})",
                self.read_capacity
            );
        }

        let open_file_flags = libc::O_NOATIME
            | if self.use_direct_io {
                libc::O_DIRECT
            } else {
                0
            };

        Ok(SequentialFileReader {
            ring,
            state: SequentialFileReaderState::default(),
            open_file_flags,
            _backing_buffer: buffer,
            _phantom: PhantomData,
        })
    }

    fn create_io_uring(&self, buf_capacity: usize) -> io::Result<IoUring> {
        // Let all buffers be submitted for reading at any time
        let max_inflight_ops = (buf_capacity / self.read_capacity as usize) as u32;

        // Completions arrive in bursts (batching done by the disk controller and the kernel).
        // By submitting smaller chunks we decrease the likelihood that we stall on a full completion queue.
        // Also, in order to keep some operations submitted at all times, we will `submit` them half-way
        // through the buffer (at the cost of doubling syscalls) to let kernel work on one half while the other
        // half is read by the user.
        let ring_squeue_size = self
            .ring_squeue_size
            .unwrap_or((max_inflight_ops / 2).max(1));
        // agave io_uring uses cqsize to define state slab size, so cqsize == max inflight ops
        let ring = sqpoll::io_uring_builder_with(self.shared_sqpoll_fd)
            .setup_cqsize(max_inflight_ops)
            .build(ring_squeue_size)?;

        // Maximum number of spawned [bounded IO, unbounded IO] kernel threads, we don't expect
        // any unbounded work, but limit it to 1 just in case (0 leaves it unlimited).
        ring.submitter()
            .register_iowq_max_workers(&mut [self.max_iowq_workers, 1])?;
        Ok(ring)
    }
}

/// Reader for non-seekable files.
///
/// Implements read-ahead using io_uring.
pub struct SequentialFileReader<'a> {
    // Note: ring's state is tied to `_backing_buffer` - contains unsafe pointer references
    // to the buffer. Ring should be drained and dropped before `_backing_buffer`.
    ring: Ring<BuffersState, ReadOp>,
    open_file_flags: i32,
    state: SequentialFileReaderState,
    /// Owned buffer used (chunked into `FixedIoBuffer` items) across lifespan of `inner`
    /// (should get dropped last)
    _backing_buffer: PageAlignedMemory,
    _phantom: PhantomData<&'a ()>,
}

impl<'a> SequentialFileReader<'a> {
    /// Open file under `path`, check its metadata to determine read limit and add it to the reader.
    ///
    /// See `add_owned_file_to_prefetch` for more details.
    pub fn set_path(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(self.open_file_flags)
            .open(path)?;
        let file_size = file.metadata()?.len();
        self.add_owned_file_to_prefetch(file, file_size)
    }

    /// Add `file` to read. Starts reading the file as soon as a buffer is available.
    ///
    /// This function uses the direct io settings set in `SequentialFileReaderBuilder`.
    ///
    /// A direct io mode reader is safe to use with non direct io files. However, passing
    /// direct io mode files to the reader in non direct io mode might result in an io error
    /// due to unaligned read.
    ///
    /// It is up to the end user to ensure that they are passing files that conform to
    /// the direct io settings of this `SequentialFileReader`.
    ///
    /// The read finishes when EOF is reached or `read_limit` bytes are read.
    /// The `read_limit` must be less than or equal to the file size when in direct io mode.
    /// Multiple files can be added to the reader and they will be read-ahead in FIFO order.
    ///
    /// Reader takes ownership of the file and will drop it after it's done reading
    /// and `move_to_next_file` is called.
    pub fn add_owned_file_to_prefetch(
        &mut self,
        file: File,
        read_limit: FileSize,
    ) -> io::Result<()> {
        self.add_file_by_fd(file.as_raw_fd(), read_limit)?;
        self.state.owned_files.push_back(file);
        Ok(())
    }

    fn add_file_to_prefetch(&mut self, file: &'a File, read_limit: FileSize) -> io::Result<()> {
        self.add_file_by_fd(file.as_raw_fd(), read_limit)
    }

    /// Caller must ensure that the file is not closed while the reader is using it.
    fn add_file_by_fd(&mut self, fd: RawFd, read_limit: FileSize) -> io::Result<()> {
        // Use `open_file_flags` to set the `is_direct_io` parameter
        self.state.files.push_back(FileState::new(
            fd,
            self.open_file_flags & libc::O_DIRECT == libc::O_DIRECT,
            read_limit,
        ));

        if self.state.all_buffers_used(self.ring.context()) {
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
        let num_bufs = self.ring.context().len();
        loop {
            self.ring.process_completions()?;
            let current_buf = self.ring.context_mut().get_mut(state.current_buf_index);
            if current_buf.is_reading() {
                // Still no data, wait for more completions, but submit in case there are queued
                // entries in the submission queue.
                self.ring.submit()?;
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
        while let Some(op) = self.state.next_read_op(self.ring.context_mut()) {
            self.ring.push(op)?;
        }
        Ok(())
    }

    fn wait_current_buf_full(&mut self) -> io::Result<bool> {
        if self.state.files.is_empty() {
            return Ok(false);
        }
        let num_bufs = self.ring.context().len();
        loop {
            self.ring.process_completions()?;

            let state = &mut self.state;
            let current_buf = &mut self.ring.context_mut().get_mut(state.current_buf_index);
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
                    self.ring.submit()?
                }

                ReadBufState::Uninit(_) => unreachable!("should be initialized"),
            }
            // Move to the next buffer and check again whether we have data.
        }
    }
}

// BufRead requires Read, but we never really use the Read interface.
impl<'a> Read for SequentialFileReader<'a> {
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

impl<'a> BufRead for SequentialFileReader<'a> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.state.current_buf_pos == self.state.current_buf_len
            && !self.wait_current_buf_full()?
        {
            return Ok(&[]);
        }

        // At this point we must have data or be at EOF.
        let current_buf = self.ring.context().get_fast(self.state.current_buf_index);
        Ok(current_buf.slice(self.state.current_buf_pos, self.state.current_buf_len))
    }

    fn consume(&mut self, amt: usize) {
        self.state.consume(amt);
    }
}

impl<'a> FileBufRead<'a> for SequentialFileReader<'a> {
    /// The `SequentialFileReader` must be in direct io mode if passing in direct io files.
    /// `read_limit` must be less than the file size if using direct io.
    /// See `add_owned_file_to_prefetch` for more details.
    fn set_file(&mut self, file: &'a File, read_limit: FileSize) -> io::Result<()> {
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

    fn get_file_offset(&self) -> FileSize {
        self.state.current_offset
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
    current_buf_pos: IoSize,
    /// Cached length of the current buffer (0 until `wait_current_buf_full` initializes it)
    current_buf_len: IoSize,
    /// File offset of the next `fill_buf()` buffer available to consume
    current_offset: FileSize,

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
        self.current_offset += amt as FileSize;

        let unconsumed_buf_len = (self.current_buf_len - self.current_buf_pos) as usize;
        if amt <= unconsumed_buf_len {
            self.current_buf_pos += amt as IoSize;
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
    /// Is the file opened with direct io
    is_direct_io: bool,
    /// Limit file offset to read up to.
    read_limit: FileSize,
    /// Offset of the next byte to read from the file
    next_read_offset: FileSize,
    /// When the file is possible to read for the first time, it should be read from this buffer index
    start_buf_index: Option<u16>,
}

impl FileState {
    fn new(raw_fd: RawFd, is_direct_io: bool, read_limit: FileSize) -> Self {
        Self {
            raw_fd,
            is_direct_io,
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
            is_direct_io,
            next_read_offset: offset,
            read_limit,
        } = self;
        let left_to_read = read_limit.saturating_sub(*offset);
        if left_to_read == 0 {
            return None;
        }

        let buf = bufs[index as usize].transition_to_reading();

        let read_len = left_to_read.min(buf.len() as FileSize);
        let op = ReadOp {
            fd: types::Fd(*raw_fd),
            buf,
            is_direct_io: *is_direct_io,
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

/// Tracks usage stages of single `IoBufferChunk` as it goes through io-uring operation
#[derive(Debug)]
enum ReadBufState {
    /// The buffer is pending submission to read queue (on initialization and
    /// in transition from `Full` to `Reading`).
    Uninit(IoBufferChunk),
    /// The buffer is currently being read and there's a corresponding ReadOp in
    /// the ring.
    Reading,
    /// The buffer is filled and ready to be consumed.
    Full {
        buf: IoBufferChunk,
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
    fn slice(&self, start_pos: IoSize, end_pos: IoSize) -> &[u8] {
        match self {
            Self::Full { buf, eof_pos } => {
                debug_assert!(eof_pos.unwrap_or(buf.len()) >= end_pos);
                let limit = (end_pos - start_pos) as usize;
                // Safety: `limit` is at most `buf.len() - start_pos` (as asserted for `end_pos`),
                // so the slice is valid given buffer's validity
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
                *self = ReadBufState::Uninit(mem::replace(buf, IoBufferChunk::empty()));
            }
        }
    }

    /// Marks the buffer as being read and returns underlying buffer to pass to `ReadOp`.
    #[must_use]
    fn transition_to_reading(&mut self) -> IoBufferChunk {
        let Self::Uninit(buf) = mem::replace(self, Self::Reading) else {
            unreachable!("buffer should be uninitialized")
        };
        buf
    }
}

#[derive(Debug)]
struct ReadOp {
    fd: types::Fd,
    buf: IoBufferChunk,
    is_direct_io: bool,
    /// This is the offset inside the buffer. It's typically 0, but can be non-zero if a previous
    /// read returned less data than requested (because of EINTR or whatever) and we submitted a new
    /// read for the remaining data.
    buf_offset: IoSize,
    /// The offset in the file.
    file_offset: FileSize,
    /// The length of the read. This is typically `read_capacity` but can be less if a previous read
    /// returned less data than requested or `file_offset` is close to the end of read limit.
    read_len: IoSize,
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
            buf,
            is_direct_io,
            buf_offset,
            file_offset,
            read_len,
            is_last_read: _,
            reader_buf_index: _,
        } = self;

        // Align the read length if necessary
        let internal_read_len = if *is_direct_io && *read_len != buf.len() {
            // Try to align the read len if possible and fall back to reading
            // the full remaining bytes if we can't align the read len.
            read_len
                .next_multiple_of(DIRECT_IO_READ_LEN_ALIGNMENT)
                .min(buf.len() - *buf_offset)
        } else {
            *read_len
        };
        debug_assert!(*buf_offset + internal_read_len <= buf.len());
        // Safety: we assert that the buffer is large enough to hold the read.
        let buf_ptr = unsafe { buf.as_mut_ptr().byte_add(*buf_offset as usize) };

        let entry = match buf.io_buf_index() {
            Some(io_buf_index) => {
                opcode::ReadFixed::new(*fd, buf_ptr, internal_read_len, io_buf_index)
                    .offset(*file_offset)
                    .ioprio(IO_PRIO_BE_HIGHEST)
                    .build()
            }
            None => opcode::Read::new(*fd, buf_ptr, internal_read_len)
                .offset(*file_offset)
                .ioprio(IO_PRIO_BE_HIGHEST)
                .build(),
        };
        entry.flags(squeue::Flags::ASYNC)
    }

    fn complete(
        &mut self,
        completion: &mut Completion<BuffersState, Self>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        let ReadOp {
            fd,
            buf,
            is_direct_io,
            buf_offset,
            file_offset,
            read_len,
            is_last_read,
            reader_buf_index,
        } = self;
        let buffers = completion.context_mut();

        let last_read_len = res? as IoSize;

        let total_read_len = *buf_offset + last_read_len;
        let buf = mem::replace(buf, IoBufferChunk::empty());

        if last_read_len > 0 && last_read_len < *read_len {
            // Partial read, retry the op with updated offsets
            let op: ReadOp = ReadOp {
                fd: *fd,
                buf,
                is_direct_io: *is_direct_io,
                buf_offset: total_read_len,
                file_offset: *file_offset + last_read_len as FileSize,
                read_len: *read_len - last_read_len,
                reader_buf_index: *reader_buf_index,
                is_last_read: *is_last_read,
            };
            // Safety:
            // The op points to a buffer which is guaranteed to be valid for the
            // lifetime of the operation
            completion.push(op)?;
        } else {
            buffers[*reader_buf_index as usize] = ReadBufState::Full {
                buf,
                eof_pos: (last_read_len == 0 || *is_last_read).then_some(total_read_len),
            };
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Seek, tempfile::NamedTempFile};

    fn read_as_vec(mut reader: impl Read) -> Vec<u8> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        buf
    }

    fn check_reading_file(
        file_size: FileSize,
        backing_buffer_size: usize,
        read_capacity: IoSize,
        use_direct_io: bool,
    ) {
        let pattern: Vec<u8> = (0..251).collect();

        // Create a temp file and write the pattern to it repeatedly
        let mut temp_file = NamedTempFile::new().unwrap();
        for _ in 0..file_size as usize / pattern.len() {
            io::Write::write_all(&mut temp_file, &pattern).unwrap();
        }
        io::Write::write_all(
            &mut temp_file,
            &pattern[..file_size as usize % pattern.len()],
        )
        .unwrap();

        let buf = PageAlignedMemory::new(backing_buffer_size).unwrap();
        let mut reader = SequentialFileReaderBuilder::new()
            .use_direct_io(use_direct_io)
            .read_capacity(read_capacity)
            .build_with_buffer(buf)
            .unwrap();
        reader.set_path(temp_file.path()).unwrap();

        // Read contents from the reader and verify length
        let all_read_data = read_as_vec(&mut reader);
        assert_eq!(all_read_data.len() as FileSize, file_size);
        assert_eq!(reader.get_file_offset(), file_size);

        // Verify the contents
        for (i, byte) in all_read_data.iter().enumerate() {
            assert_eq!(*byte, pattern[i % pattern.len()], "Mismatch - pos {i}");
        }
    }

    /// Test with buffer larger than the whole file
    #[test]
    fn test_reading_small_file() {
        check_reading_file(2500, 4096, 1024, false);
        check_reading_file(2500, 4096, 2048, false);
        check_reading_file(2500, 4096, 4096, false);
    }

    /// Test with buffer smaller than the whole file
    #[test]
    fn test_reading_file_in_chunks() {
        check_reading_file(25_000, 16384, 1024, false);
        check_reading_file(25_000, 4096, 1024, false);
        check_reading_file(25_000, 4096, 2048, false);
        check_reading_file(25_000, 4096, 4096, false);
    }

    /// Test with buffer much smaller than the whole file
    #[test]
    fn test_reading_large_file() {
        check_reading_file(250_000, 32768, 1024, false);
        check_reading_file(250_000, 16384, 1024, false);
        check_reading_file(250_000, 4096, 1024, false);
        check_reading_file(250_000, 4096, 2048, false);
        check_reading_file(250_000, 4096, 4096, false);
    }

    #[test]
    fn test_non_registered_buffer_read() {
        let file_size = 64 * 1024;
        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        let data = (0..).take(file_size).map(|v| v as u8).collect::<Vec<_>>();
        io::Write::write_all(temp_file.as_file_mut(), &data).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_capacity(4 * 1024)
            .use_registered_buffers(false)
            .build(16 * 1024)
            .unwrap();
        reader.set_path(temp_file.path()).unwrap();

        let mut all_read_data = Vec::new();
        reader.read_to_end(&mut all_read_data).unwrap();
        assert_eq!(all_read_data.len(), file_size);
        assert_eq!(all_read_data, data);
    }

    #[test]
    fn test_add_file_ref() {
        let mut temp_file = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp_file, &[0xa, 0xb, 0xc]).unwrap();
        temp_file.rewind().unwrap();

        {
            let mut reader = SequentialFileReaderBuilder::new()
                .read_capacity(512)
                .build(1024)
                .unwrap();
            reader.add_file_to_prefetch(temp_file.as_file(), 3).unwrap();
            assert_eq!(read_as_vec(&mut reader), &[0xa, 0xb, 0xc]);
        }
        // Independently we can also read from the file directly
        assert_eq!(read_as_vec(&mut temp_file), &[0xa, 0xb, 0xc]);
    }

    #[test]
    fn test_direct_io_read() {
        check_reading_file(2_500, 4096, 4096, true);
        check_reading_file(2_500, 16384, 4096, true);
        check_reading_file(25_000, 4096, 4096, true);
        check_reading_file(25_000, 16384, 4096, true);
        check_reading_file(250_000, 4096, 4096, true);
        check_reading_file(250_000, 16384, 4096, true);
        check_reading_file(4096, 4096, 4096, true);
        check_reading_file(4096, 16384, 4096, true);
        check_reading_file(16384, 4096, 4096, true);
        check_reading_file(16384, 16384, 4096, true);
    }

    #[test]
    fn test_multiple_unlimited_files() {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0xd, 0xe, 0xf, 0x10]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_capacity(512)
            .build(1024)
            .unwrap();

        let f1 = File::open(temp1.path()).unwrap();
        let f2 = File::open(temp2.path()).unwrap();
        reader
            .add_owned_file_to_prefetch(f1, FileSize::MAX)
            .unwrap();
        reader
            .add_owned_file_to_prefetch(f2, FileSize::MAX)
            .unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);
        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
        reader.move_to_next_file().unwrap();

        let f1 = File::open(temp1.path()).unwrap();
        reader
            .add_owned_file_to_prefetch(f1, FileSize::MAX)
            .unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);
    }

    #[test]
    fn test_get_offset() {
        let pattern = (0..600).map(|i| i as u8).collect::<Vec<_>>();
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &pattern).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_capacity(512)
            .build(1024)
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
            .read_capacity(512)
            .build(2048)
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
    fn test_set_file() {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0xd, 0xe, 0xf, 0x10]).unwrap();

        let mut reader = SequentialFileReaderBuilder::new()
            .read_capacity(512)
            .build(1024)
            .unwrap();
        reader.add_file_to_prefetch(temp1.as_file(), 3).unwrap();
        reader.add_file_to_prefetch(temp2.as_file(), 4).unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        reader.set_file(temp2.as_file(), 4).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);

        reader.set_file(temp1.as_file(), 4).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        let f1 = File::open(temp1.path()).unwrap();
        reader
            .add_owned_file_to_prefetch(f1, FileSize::MAX)
            .unwrap();
        reader.move_to_next_file().unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        reader.set_file(temp2.as_file(), 4).unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
    }
}
