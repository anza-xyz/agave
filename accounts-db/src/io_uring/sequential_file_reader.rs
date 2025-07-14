use {
    super::{
        memory::{FixedIoBuffer, LargeBuffer},
        IO_PRIO_BE_HIGHEST,
    },
    crate::buffered_reader::ContiguousBufFileRead,
    agave_io_uring::{Completion, Ring, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    std::{
        collections::VecDeque,
        fs::{File, OpenOptions},
        io::{self, BufRead, Read},
        marker::PhantomData,
        mem,
        os::{
            fd::{AsRawFd as _, RawFd},
            unix::fs::OpenOptionsExt,
        },
        path::Path,
    },
};

// Based on transfers seen with `dd bs=SIZE` for NVME drives: values >=64KiB are fine,
// but peak at 1MiB. Also compare with particular NVME parameters, e.g.
// 32 pages (Maximum Data Transfer Size) * page size (MPSMIN = Memory Page Size) = 128KiB.
const DEFAULT_READ_SIZE: usize = 1024 * 1024;
const SQPOLL_IDLE_TIMEOUT: u32 = 50;
// For large file we don't really use workers as few regularly submitted requests get handled
// within sqpoll thread. Allow some workers just in case, but limit them.
const MAX_IOWQ_WORKERS: u32 = 2;

/// Reader for non-seekable files.
///
/// Implements read-ahead using io_uring.
pub struct SequentialFileReader<'a, B> {
    // Note: state is tied to `backing_buffer` and `owned_files` - contains unsafe pointer references
    // to the buffer and operates on file descriptors of files that are assumed to be open.
    inner: Ring<SequentialFileReaderState, ReadOp>,
    owned_files: VecDeque<File>,
    /// Owned buffer used (chunked into `FixedIoBuffer` items) across lifespan of `inner`
    /// (should get dropped last)
    _backing_buffer: B,
    _phantom: PhantomData<&'a ()>,
}

impl SequentialFileReader<'_, LargeBuffer> {
    /// Create a new `SequentialFileReader` using internally allocated buffer
    /// of specified `buf_size` and default read size.
    pub fn with_capacity(buf_size: usize) -> io::Result<Self> {
        Self::with_buffer(LargeBuffer::new(buf_size), DEFAULT_READ_SIZE)
    }
}

impl<B: AsMut<[u8]>> SequentialFileReader<'_, B> {
    /// Create a new `SequentialFileReader` using provided backing `buffer`.
    ///
    /// `buffer` is the internal buffer used for reading. It must be at least `read_capacity` long.
    /// The reader will execute multiple `read_capacity` sized reads in parallel to fill the buffer.
    ///
    /// Initially the reader is idle and starts reading after the first file is added.
    /// # Example:
    /// ```
    /// let mut buffer = vec![0; 4096];
    /// let mut reader = SequentialFileReader::with_buffer(buffer, 1024);
    /// let file = File::open("example.txt").unwrap();
    /// reader.add_file(file).unwrap();
    /// assert!(!reader.fill_buf().unwrap().is_empty());
    /// ```
    pub fn with_buffer(mut buffer: B, read_capacity: usize) -> io::Result<Self> {
        // Let submission queue hold half of buffers before we explicitly syscall
        // to submit them for reading.
        let ring_qsize = (buffer.as_mut().len() / read_capacity / 2).max(1) as u32;
        let ring = IoUring::builder()
            .setup_sqpoll(SQPOLL_IDLE_TIMEOUT)
            .build(ring_qsize)?;
        // Maximum number of spawned [bounded IO, unbounded IO] kernel threads, we don't expect
        // any unbounded work, but limit it to 1 just in case (0 leaves it unlimited).
        ring.submitter()
            .register_iowq_max_workers(&mut [MAX_IOWQ_WORKERS, 1])?;
        Self::with_buffer_and_ring(buffer, ring, read_capacity)
    }

    /// Create a new `SequentialFileReader` using a custom ring instance.
    fn with_buffer_and_ring(
        mut backing_buffer: B,
        ring: IoUring,
        read_capacity: usize,
    ) -> io::Result<Self> {
        let buffer = backing_buffer.as_mut();
        assert!(buffer.len() >= read_capacity, "buffer too small");
        assert!(
            buffer.len() % read_capacity == 0,
            "buffer size must be a multiple of read_capacity"
        );

        // Safety: buffers contain unsafe pointers to `buffer`, but we make sure they are
        // dropped before `backing_buffer` is dropped.
        let buffers = unsafe { FixedIoBuffer::split_buffer_chunks(buffer, read_capacity) }
            .map(ReadBufState::Uninit)
            .collect();
        let inner = Ring::new(
            ring,
            SequentialFileReaderState {
                buffers,
                files: VecDeque::new(),
                next_read_file_index: None,
                next_read_buf_index: 0,
            },
        );

        // Safety: kernel holds unsafe pointers to `buffer`, struct field layout guarantees
        // that the ring is destroyed before `backing_buffer` is dropped.
        unsafe { FixedIoBuffer::register(buffer, &inner)? };

        Ok(Self {
            inner,
            owned_files: VecDeque::new(),
            _backing_buffer: backing_buffer,
            _phantom: PhantomData,
        })
    }
}

impl<'a, B> SequentialFileReader<'a, B> {
    /// Opens file under `path`, check its metadata to determine read limit and add it to the reader.
    ///
    /// See `add_file` for more details.
    pub fn add_path(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOATIME)
            .open(path)?;
        let len = file.metadata()?.len() as usize;
        self.add_file(file, Some(len))
    }

    /// Add `file` to read. Starts reading the file as soon as a buffer is available.
    /// The read finishes when EOF is reached or `limit_len` bytes are read (if specified).
    /// Multiple files can be added to the reader and they will be read-ahead in FIFO order.
    ///
    /// Reader takes ownership of the file and will drop it after it's done reading
    /// and `move_to_next_file` is called.
    pub fn add_file(&mut self, file: File, limit_len: Option<usize>) -> io::Result<()> {
        self.add_file_by_fd(file.as_raw_fd(), limit_len)?;
        self.owned_files.push_back(file);
        Ok(())
    }

    /// Add `file` reference to read. Starts reading the file as soon as a buffer is available.
    /// Multiple files can be added to the reader and they will be read-ahead in FIFO order.
    ///
    /// Lifetime of reference is tied to the reader's lifetime.
    #[allow(unused)]
    pub fn add_file_ref(&mut self, file: &'a File, limit_len: Option<usize>) -> io::Result<()> {
        self.add_file_by_fd(file.as_raw_fd(), limit_len)
    }

    /// When reading multiple files, this method moves the reader to the next file.
    ///
    /// It is required that the previous file is fully read before calling this method.
    #[allow(unused)]
    pub fn move_to_next_file(&mut self) -> io::Result<()> {
        let state = self.inner.context_mut();

        let Some(file_state) = state.files.pop_front() else {
            return Ok(());
        };
        if let Some(next_file_index) = state.next_read_file_index.as_mut() {
            state.next_read_file_index = next_file_index.checked_sub(1);
        }

        if let Some(buf_index) = file_state.current_buf_index {
            let current_buf = &mut state.buffers[buf_index];
            assert!(current_buf.seen_eof(), "must fully read file first");

            current_buf.transition_to_uninit();
        }

        if file_state.read_limit.is_none() {
            // This was unlimited file read, so no other files are queued,
            // but some ops might still be in-flight, drain them.
            self.inner.drain()?;
            self.inner.context_mut().reset_buffers();
            if !self.inner.context().files.is_empty() {
                self.inner.context_mut().next_read_file_index = Some(0);
            }
        }

        // Start reading as many buffers as necessary for queued files.
        while let Some(op) = self.inner.context_mut().next_read_op() {
            self.inner.push(op)?;
        }
        // Make sure work is started in case submission queue is large and we
        // never submitted work when adding buffers.
        self.inner.submit()?;

        if self
            .owned_files
            .front()
            .is_some_and(|f| f.as_raw_fd() == file_state.raw_fd)
        {
            self.owned_files.pop_front();
        }
        Ok(())
    }

    /// Caller must ensure that the file is not closed while the reader is using it.
    fn add_file_by_fd(&mut self, fd: RawFd, limit_len: Option<usize>) -> io::Result<()> {
        let state = self.inner.context_mut();

        state.files.push_back(FileState::new(fd, limit_len));

        if state.all_buffers_used() {
            // Just added file to backlog, no reads can be started yet.
            return Ok(());
        }

        // There are free buffers, so we can start reading the new file.
        state.next_read_file_index = Some(state.next_read_file_index.map_or(0, |idx| idx + 1));

        // Start reading as many buffers as necessary for queued files.
        while let Some(op) = self.inner.context_mut().next_read_op() {
            self.inner.push(op)?;
        }
        // Make sure work is started in case submission queue is large and we
        // never submitted work when adding buffers.
        self.inner.submit()
    }
}

impl<'a, B: AsMut<[u8]>> ContiguousBufFileRead for SequentialFileReader<'a, B> {
    fn get_file_offset(&self) -> usize {
        match self.inner.context().files.front() {
            Some(file) => file.current_offset,
            None => 0,
        }
    }

    fn set_default_required_fill_buf_len(&mut self, _len: usize) {
        todo!()
    }

    fn set_next_required_fill_buf_len(&mut self, _len: usize) {
        todo!()
    }
}

/// Holds the state of a single file being read.
struct FileState {
    raw_fd: RawFd,
    /// Limit file offset to read up to.
    read_limit: Option<usize>,
    /// Offset of the next byte to read from file
    next_read_offset: usize,
    /// File offset of the next `fill_buf()` buffer available to consume
    current_offset: usize,
    /// Position within the current buffer to return at next `fill_buf()`
    current_buf_pos: usize,
    /// Index of `state.buffers` to consume data from (if file is already being read)
    current_buf_index: Option<usize>,
}

impl FileState {
    fn new(raw_fd: RawFd, read_limit: Option<usize>) -> Self {
        Self {
            raw_fd,
            read_limit,
            next_read_offset: 0,
            current_offset: 0,
            current_buf_pos: 0,
            current_buf_index: None,
        }
    }

    /// Create a new read operation into the `bufs[index]` buffer and update file state.
    ///
    /// This is called at start and as soon as a buffer is fully consumed by BufRead::fill_buf().
    ///
    /// Returns `ReadOp` that will read [self.offset, self.offset + min(buf len, read limit))
    /// from the file into `bufs[index]`. Once the read is complete the buffer changes into
    /// `Full` state and can be consumed (once `self.current_buf_index` points to it).
    fn next_read_op(&mut self, index: usize, bufs: &mut [ReadBufState]) -> Option<ReadOp> {
        let Self {
            current_offset: _,
            current_buf_pos: _,
            current_buf_index,
            raw_fd,
            next_read_offset: offset,
            read_limit,
        } = self;
        let left_to_read = if let Some(limit_offset) = read_limit {
            if *limit_offset <= *offset {
                return None;
            }
            *limit_offset - *offset
        } else {
            usize::MAX
        };

        let buf = bufs[index].transition_to_reading();

        let read_len = left_to_read.min(buf.len());
        let op = ReadOp {
            fd: types::Fd(*raw_fd),
            buf,
            buf_offset: 0,
            file_offset: *offset,
            read_len,
            is_last_read: left_to_read == read_len,
            reader_buf_index: index,
        };
        // Mark file state to start reading at `index` buffer
        if current_buf_index.is_none() {
            *current_buf_index = Some(index);
        }

        // We always advance by `read_len`. If we get a short read, we submit a new
        // read for the remaining data. See ReadOp::complete().
        *offset += read_len;

        Some(op)
    }
}

/// Holds the state of the reader.
struct SequentialFileReaderState {
    buffers: Vec<ReadBufState>,
    files: VecDeque<FileState>,
    next_read_file_index: Option<usize>,
    next_read_buf_index: usize,
}

impl SequentialFileReaderState {
    /// The front buffer index (of `self.buffers`) where already read bytes of current file
    /// are available.
    fn current_buf_index(&self) -> Option<usize> {
        self.files.front().and_then(|f| f.current_buf_index)
    }

    fn current_buf(&self) -> Option<&ReadBufState> {
        self.current_buf_index().map(|idx| &self.buffers[idx])
    }

    fn current_file_and_buf(&self) -> Option<(&FileState, &ReadBufState)> {
        self.files
            .front()
            .and_then(|f| f.current_buf_index.map(|idx| (f, &self.buffers[idx])))
    }

    fn current_file_and_buf_mut(&mut self) -> Option<(&mut FileState, &mut ReadBufState)> {
        self.files
            .front_mut()
            .and_then(|f| f.current_buf_index.map(|idx| (f, &mut self.buffers[idx])))
    }

    /// Returns the next read operation for the reader.
    /// If all buffers are used, returns `None`.
    ///
    /// Reads are issued for files added into the reader from first file at position 0
    /// to its limit / EOF and then for any subsequent files.
    fn next_read_op(&mut self) -> Option<ReadOp> {
        if self.all_buffers_used() {
            return None;
        }
        loop {
            let read_file_index = self.next_read_file_index?;
            match self.files[read_file_index]
                .next_read_op(self.next_read_buf_index, &mut self.buffers)
            {
                Some(op) => {
                    self.next_read_buf_index = (self.next_read_buf_index + 1) % self.buffers.len();
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

    /// Returns `true` if there are no more buffers available for reading.
    fn all_buffers_used(&mut self) -> bool {
        self.buffers[self.next_read_buf_index].is_used()
    }

    fn reset_buffers(&mut self) {
        self.buffers
            .iter_mut()
            .for_each(|buf| buf.transition_to_uninit());
    }
}

// BufRead requires Read, but we never really use the Read interface.
impl<B: AsMut<[u8]>> Read for SequentialFileReader<'_, B> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        if available.is_empty() {
            return Ok(0); // EOF.
        }

        let bytes_to_read = available.len().min(buf.len());
        buf[..bytes_to_read].copy_from_slice(&available[..bytes_to_read]);
        self.consume(bytes_to_read);
        Ok(bytes_to_read)
    }
}

impl<B: AsMut<[u8]>> BufRead for SequentialFileReader<'_, B> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let _have_data = loop {
            let state = self.inner.context_mut();
            let num_bufs = state.buffers.len();
            let Some((current_file, current_buf)) = state.current_file_and_buf_mut() else {
                return Ok(&[]);
            };
            match current_buf {
                ReadBufState::Full { buf, eof_pos } => {
                    let pos = current_file.current_buf_pos;
                    if pos < buf.len() && eof_pos.is_none_or(|eof| pos < eof) {
                        // We have some data available.
                        break true;
                    }

                    if eof_pos.is_some() {
                        // Last filled buf for the whole file (until `move_to_next_file` is called).
                        return Ok(&[]);
                    }
                    // We have finished consuming this buffer - reset its state.
                    current_buf.transition_to_uninit();

                    // Next `fill_buf` will use subsequent buffer.
                    if let Some(idx) = current_file.current_buf_index.as_mut() {
                        *idx = (*idx + 1) % num_bufs
                    }
                    current_file.current_buf_pos = 0;

                    // A buffer was freed, so try to queue up next read.
                    if let Some(op) = state.next_read_op() {
                        self.inner.push(op)?;
                    }

                    // Move to the next buffer and check again whether we have data.
                    continue;
                }
                ReadBufState::Uninit(_) => unreachable!("should be initialized"),
                _ => break false,
            }
        };

        loop {
            self.inner.process_completions()?;
            match self.inner.context().current_buf().unwrap() {
                ReadBufState::Full { .. } => break,
                ReadBufState::Uninit(_) => unreachable!("should be initialized"),
                // Still no data, wait for more completions, but submit in case the SQPOLL
                // thread is asleep and there are queued entries in the submission queue.
                ReadBufState::Reading => self.inner.submit()?,
            }
        }

        // At this point we must have data or be at EOF.
        match self.inner.context().current_file_and_buf().unwrap() {
            (file_state, ReadBufState::Full { buf, eof_pos }) => {
                Ok(buf.slice_range(file_state.current_buf_pos, eof_pos))
            }
            // after the loop above we either have some data or we must be at EOF
            _ => unreachable!(),
        }
    }

    fn consume(&mut self, amt: usize) {
        let state = self.inner.context_mut();
        let Some(file_state) = state.files.front_mut() else {
            return;
        };
        file_state.current_offset += amt;
        if let Some(buf_index) = file_state.current_buf_index {
            match &mut state.buffers[buf_index] {
                ReadBufState::Full { buf, .. } => {
                    debug_assert!(
                        file_state.current_buf_pos + amt <= buf.len(),
                        "consume beyond buffer not supported"
                    );
                    file_state.current_buf_pos += amt;
                }
                _ => (),
            };
        } else {
            file_state.next_read_offset = file_state.current_offset;
        }
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
        eof_pos: Option<usize>,
    },
}

impl ReadBufState {
    fn is_used(&self) -> bool {
        matches!(self, ReadBufState::Reading | ReadBufState::Full { .. })
    }

    fn seen_eof(&self) -> bool {
        match self {
            Self::Full { eof_pos, .. } => eof_pos.is_some(),
            Self::Uninit(_) | Self::Reading => false,
        }
    }

    /// Marks the buffer as uninitialized (after it has been fully consumed).
    fn transition_to_uninit(&mut self) {
        match self {
            Self::Uninit(_) => (),
            Self::Reading => unreachable!("cannot reset a buffer that is pending read"),
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
    buf_offset: usize,
    /// The offset in the file.
    file_offset: usize,
    /// The length of the read. This is typically `read_capacity` but can be less if a previous read
    /// returned less data than requested or `file_offset` is close to the end of read limit.
    read_len: usize,
    /// Indicates that after reading `read_len` we have reached configured read limit.
    is_last_read: bool,
    /// This is the index of the buffer in the reader's state. It's used to update the state once the
    /// read completes.
    reader_buf_index: usize,
}

impl RingOp<SequentialFileReaderState> for ReadOp {
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
            unsafe { buf.as_mut_ptr().byte_add(buf_offset) },
            read_len as u32,
            buf.io_buf_index()
                .expect("should have a valid fixed buffer"),
        )
        .offset(file_offset as u64)
        .ioprio(IO_PRIO_BE_HIGHEST)
        .build()
    }

    fn complete(
        &mut self,
        completion: &mut Completion<SequentialFileReaderState, Self>,
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
        let reader_state = completion.context_mut();

        let last_read_len = res? as usize;

        let total_read_len = buf_offset + last_read_len;
        let buf = mem::replace(buf, FixedIoBuffer::empty());

        if last_read_len > 0 && last_read_len < read_len {
            // Partial read, retry the op with updated offsets
            let op: ReadOp = ReadOp {
                fd,
                buf,
                buf_offset: total_read_len,
                file_offset: file_offset + last_read_len,
                read_len: read_len - last_read_len,
                reader_buf_index,
                is_last_read,
            };
            // Safety:
            // The op points to a buffer which is guaranteed to be valid for the
            // lifetime of the operation
            completion.push(op);
        } else {
            reader_state.buffers[reader_buf_index] = ReadBufState::Full {
                buf,
                eof_pos: (last_read_len == 0 || is_last_read).then_some(total_read_len),
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

    fn check_reading_file(file_size: usize, backing_buffer_size: usize, read_capacity: usize) {
        let pattern: Vec<u8> = (0..251).collect();

        // Create a temp file and write the pattern to it repeatedly
        let mut temp_file = NamedTempFile::new().unwrap();
        for _ in 0..file_size / pattern.len() {
            io::Write::write_all(&mut temp_file, &pattern).unwrap();
        }
        io::Write::write_all(&mut temp_file, &pattern[..file_size % pattern.len()]).unwrap();

        let buf = vec![0; backing_buffer_size];
        let mut reader = SequentialFileReader::with_buffer(buf, read_capacity).unwrap();
        reader
            .add_file(File::open(temp_file.path()).unwrap(), None)
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
            let mut reader = SequentialFileReader::with_buffer(vec![0; 1024], 512).unwrap();
            reader.add_file_ref(&temp_file.as_file(), Some(3)).unwrap();
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

        let mut reader = SequentialFileReader::with_buffer(vec![0; 1024], 512).unwrap();

        let f1 = File::open(temp1.path()).unwrap();
        let f2 = File::open(temp2.path()).unwrap();
        reader.add_file(f1, None).unwrap();
        reader.add_file(f2, None).unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        reader.move_to_next_file().unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
    }

    #[test]
    fn test_multiple_limited_files() {
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &[0xa, 0xb, 0xc]).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &[0xd, 0xe, 0xf, 0x10]).unwrap();

        let mut reader = SequentialFileReader::with_buffer(vec![0; 1024], 512).unwrap();
        reader.add_file_ref(temp1.as_file(), Some(2)).unwrap();
        reader.add_file_ref(temp2.as_file(), Some(3)).unwrap();
        reader.add_file_ref(temp1.as_file(), Some(4)).unwrap();
        reader.add_file_ref(temp2.as_file(), Some(5)).unwrap();

        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb]);

        reader.move_to_next_file().unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf]);

        reader.move_to_next_file().unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xa, 0xb, 0xc]);

        reader.move_to_next_file().unwrap();
        assert_eq!(read_as_vec(&mut reader), vec![0xd, 0xe, 0xf, 0x10]);
    }

    #[test]
    fn test_multiple_medium_limited_files() {
        let pattern = (0..2000).map(|i| i as u8).collect::<Vec<_>>();
        let mut temp1 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp1, &pattern).unwrap();
        let mut temp2 = NamedTempFile::new().unwrap();
        io::Write::write_all(&mut temp2, &pattern[1000..]).unwrap();

        let mut reader = SequentialFileReader::with_buffer(vec![0; 1024], 512).unwrap();
        reader.add_file_ref(temp1.as_file(), Some(1990)).unwrap();
        reader.add_file_ref(temp2.as_file(), Some(1000)).unwrap();
        reader.add_file_ref(temp1.as_file(), Some(2010)).unwrap();

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

        let mut reader = SequentialFileReader::with_buffer(vec![0; 1024], 512).unwrap();
        reader.add_file_ref(temp1.as_file(), Some(1990)).unwrap();
        assert_eq!(read_as_vec(&mut reader), &pattern[..1990]);
        reader.move_to_next_file().unwrap();

        for _ in 0..10 {
            reader.add_file_ref(temp2.as_file(), Some(1000)).unwrap();
            assert_eq!(read_as_vec(&mut reader), &pattern[1000..]);
            reader.move_to_next_file().unwrap();

            reader.add_file_ref(temp1.as_file(), Some(2010)).unwrap();
            assert_eq!(read_as_vec(&mut reader), &pattern[..2000]);
            reader.move_to_next_file().unwrap();
        }
        assert_eq!(read_as_vec(&mut reader), Vec::<u8>::new());

        for _ in 0..10 {
            reader.add_file_ref(temp2.as_file(), Some(1000)).unwrap();
            reader.add_file_ref(temp1.as_file(), Some(2010)).unwrap();

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

        let mut reader = SequentialFileReader::with_buffer(vec![0; 1024], 512).unwrap();
        reader.add_file_ref(temp1.as_file(), Some(1990)).unwrap();

        assert_eq!(512, reader.fill_buf().unwrap().len());
        assert_eq!(0, reader.get_file_offset());

        reader.consume(40);
        assert_eq!(40, reader.get_file_offset());
        assert_eq!(472, reader.fill_buf().unwrap().len());

        reader.consume(472);
        assert_eq!(512, reader.get_file_offset());
        assert_eq!(88, reader.fill_buf().unwrap().len());

        reader.consume(88);
        assert_eq!(600, reader.get_file_offset());
        assert_eq!(0, reader.fill_buf().unwrap().len());
    }
}
