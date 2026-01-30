#![allow(clippy::arithmetic_side_effects)]

use {
    super::{
        memory::{IoBufferChunk, PageAlignedMemory},
        IO_PRIO_BE_HIGHEST,
    },
    crate::{
        io_uring::{file_creator::CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT, sqpoll},
        FileSize, IoSize,
    },
    agave_io_uring::{Completion, Ring, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    std::{
        collections::VecDeque,
        fs::{File, OpenOptions},
        io::{self, Cursor},
        mem,
        os::{
            fd::{AsRawFd, BorrowedFd},
            unix::fs::OpenOptionsExt,
        },
        path::Path,
    },
};

// Based on benches done with sequential writes
// This value returned the best write speed on a modern SSD
// with the EXT4 fs
pub const DEFAULT_WRITE_SIZE: IoSize = 1024 * 1024;
// For large file we don't really use workers as few regularly submitted requests get handled
// within sqpoll thread. Allow some workers just in case, but limit them.
const DEFAULT_MAX_IOWQ_WORKERS: u32 = 2;
// In testing, the blocksize returned by metadata is always 4096, even when the actual
// blocksize of the filesystem is 512. We can't tell without using statx, which is
// currently not supported, so we just always use 4096 to be safe.
const DEFAULT_FS_BLOCK_SIZE: usize = 4096;

/// Utility for building `IoUringFileWriter` with specified tuning options.
pub struct IoUringFileWriterBuilder<'sp> {
    write_size: IoSize,
    max_iowq_workers: u32,
    ring_squeue_size: Option<u32>,
    shared_sqpoll_fd: Option<BorrowedFd<'sp>>,
    /// Register buffer as fixed with the kernel
    register_buffer: bool,
    /// Toggle option for opening files with the O_DIRECT flag
    use_direct_io: bool,
}

impl<'sp> IoUringFileWriterBuilder<'sp> {
    pub fn new() -> Self {
        Self {
            write_size: DEFAULT_WRITE_SIZE,
            max_iowq_workers: DEFAULT_MAX_IOWQ_WORKERS,
            ring_squeue_size: None,
            shared_sqpoll_fd: None,
            register_buffer: false,
            use_direct_io: false,
        }
    }

    /// Override the default size of a single IO write operation
    ///
    /// This influences the concurrency, since buffer is divided into chunks of this size.
    #[cfg(test)]
    pub fn write_size(mut self, write_size: IoSize) -> Self {
        self.write_size = write_size;
        self
    }

    /// Set whether to register buffer with `io_uring` for improved performance.
    ///
    /// Enabling requires available memlock ulimit to be higher than sizes of registered buffers.
    pub fn _use_registered_buffers(mut self, register_buffers: bool) -> Self {
        self.register_buffer = register_buffers;
        self
    }

    /// Set whether to use directio when writing
    ///
    /// Enabling requires the filesystem to support directio and subbuffers to be a multiple
    /// of the fs block size
    #[cfg(test)]
    pub fn use_direct_io(mut self, use_direct_io: bool) -> Self {
        self.use_direct_io = use_direct_io;
        self
    }

    /// Use (or remove) a shared kernel thread to drain submission queue for IO operations
    pub fn _shared_sqpoll(mut self, shared_sqpoll_fd: Option<BorrowedFd<'sp>>) -> Self {
        self.shared_sqpoll_fd = shared_sqpoll_fd;
        self
    }

    /// Build a new `IoUringFileWriter` with internally allocated buffer.
    ///
    /// Buffer will hold at least `buf_capacity` bytes (increased to `write_size` if it's lower).
    pub fn build(
        self,
        path: impl AsRef<Path>,
        buf_capacity: usize,
    ) -> io::Result<IoUringFileWriter> {
        let buf_capacity = buf_capacity.max(self.write_size as usize);
        let buffer = PageAlignedMemory::new(buf_capacity)?;
        self.build_with_buffer(path, buffer)
    }

    /// Build a new `IoUringFileWriter` with a user-supplied buffer
    ///
    /// `buffer` is the internal buffer used for writing. It must be at least `write_size` long.
    ///
    /// The writer will execute multiple `write_size` writes in parallel to fill the buffer.
    fn build_with_buffer(
        self,
        path: impl AsRef<Path>,
        mut buffer: PageAlignedMemory,
    ) -> io::Result<IoUringFileWriter> {
        // Align buffer capacity to write capacity, so we always write equally sized chunks
        let buf_capacity =
            buffer.as_mut().len() / self.write_size as usize * self.write_size as usize;
        assert_ne!(buf_capacity, 0, "write size aligned buffer is too small");
        let buf_slice_mut = &mut buffer.as_mut()[..buf_capacity];

        // Safety: buffers contain unsafe pointers to `buffer`, but we make sure they are
        // dropped before `backing_buffer` in `IoUringFileWriter` is dropped.
        let buffers = unsafe {
            IoBufferChunk::split_buffer_chunks(buf_slice_mut, self.write_size, self.register_buffer)
        }
        .map(Cursor::new)
        .collect();

        #[cfg(debug_assertions)]
        if self.use_direct_io {
            // O_DIRECT writes have size and alignment restrictions and must be into a sub-buffer of
            // some multiple of the fs block size (see https://man7.org/linux/man-pages/man2/open.2.html#NOTES).
            assert!(
                self.write_size.is_multiple_of(DEFAULT_FS_BLOCK_SIZE as u32),
                "write size is not multiple of fs block size={DEFAULT_FS_BLOCK_SIZE}"
            );
        }

        let state = IoUringFileWriterState::new(path, buffers, self.use_direct_io)?;

        let io_uring = self.create_io_uring(buf_capacity)?;
        let ring = Ring::new(io_uring, state);

        if self.register_buffer {
            // Safety: kernel holds unsafe pointers to `buffer`, struct field declaration order
            // guarantees that the ring is destroyed before `_backing_buffer` is dropped.
            unsafe { IoBufferChunk::register(buf_slice_mut, &ring)? };
        }

        let writer = IoUringFileWriter {
            ring,
            _backing_buffer: buffer,
        };
        Ok(writer)
    }

    fn create_io_uring(&self, buf_capacity: usize) -> io::Result<IoUring> {
        // Let all buffers be submitted for writing at any time
        let max_inflight_ops = (buf_capacity / self.write_size as usize) as u32;

        // Completions arrive in bursts (batching done by the disk controller and the kernel).
        // By submitting smaller chunks we decrease the likelihood that we stall on a full completion queue.
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

/// Writer that uses io_uring.
pub struct IoUringFileWriter {
    // Note: state is tied to `backing_buffer` and contains unsafe pointer references to it
    ring: Ring<IoUringFileWriterState, WriteOp>,
    /// Owned buffer used (chunked into `IoBufferChunk` items) across lifespan of `inner`
    /// (should get dropped last)
    _backing_buffer: PageAlignedMemory,
}

impl IoUringFileWriter {
    /// Flush the provided buffer and begin writing it
    fn internal_flush(&mut self, cursor: Cursor<IoBufferChunk>) -> io::Result<()> {
        let context = self.ring.context_mut();
        let offset = context.offset;
        // write only the portion of the buffer with data
        let write_len = cursor.position();
        // use direct io if possible
        let fd = if write_len.is_multiple_of(DEFAULT_FS_BLOCK_SIZE as u64)
            && context.direct_io_file.is_some()
        {
            context.stats.direct_io_write_count += 1;
            io_uring::types::Fd(context.direct_io_file.as_ref().unwrap().as_raw_fd())
        } else {
            context.stats.regular_io_write_count += 1;
            io_uring::types::Fd(context.file.as_raw_fd())
        };
        context.stats.num_writes_total += 1;
        context.stats.num_buffers_free_total += context.buffers.len();
        context.offset = offset + write_len;
        let op = WriteOp {
            fd,
            offset,
            buf: cursor.into_inner(),
            buf_offset: 0,
            write_len: write_len as u32,
        };
        self.ring.push(op)?;
        self.ring.submit()?;

        Ok(())
    }

    fn drain(&mut self) -> io::Result<()> {
        let res = self.ring.drain();
        self.ring.context().log_stats();
        res
    }
}

impl io::Write for IoUringFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut num_bytes_already_written: usize = 0;
        loop {
            let state = self.ring.context_mut();
            if let Some(cursor) = state.buffers.front_mut() {
                let cursor_position = cursor.position();
                // copy as many bytes as we can into the buffer without overflowing
                let write_len: usize = std::cmp::min(
                    (cursor.get_ref().len() as usize - cursor_position as usize) as usize,
                    buf.len() - num_bytes_already_written as usize,
                );
                cursor.get_mut().as_mut()
                    [cursor_position as usize..cursor_position as usize + write_len]
                    .copy_from_slice(
                        &buf[num_bytes_already_written..num_bytes_already_written + write_len],
                    );
                cursor.set_position(cursor_position + write_len as u64);
                num_bytes_already_written += write_len;

                // if buffer is full, flush the buffer
                if cursor.position() == cursor.get_ref().len() as u64 {
                    // take the buffer on the front of the queue
                    let cursor = state.buffers.pop_front().unwrap();
                    self.internal_flush(cursor)?;
                }

                // if finished writing bytes, then break
                // otherwise, find the next free buffer to keep writing bytes
                if num_bytes_already_written == buf.len() {
                    break;
                }
            } else {
                // only process completions + submit and wait if we were unable to obtain a free buffer
                // process completions is quite expensive if there's a lot of small writes, and we don't
                // actually need to run it every time we write, only when we want some buffers to be freed
                // back into the free buffer queue
                self.ring.process_completions()?;
                self.ring
                    .submit_and_wait(1, CHECK_PROGRESS_AFTER_SUBMIT_TIMEOUT)?;
            }
        }

        Ok(num_bytes_already_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush any partial buffer (current write target)
        let state = self.ring.context_mut();
        if let Some(cursor) = state.buffers.front() {
            if cursor.position() > 0 {
                let cursor = state.buffers.pop_front().unwrap();
                self.internal_flush(cursor)?;
            }
        }

        self.drain()?;

        Ok(())
    }
}

impl io::Seek for IoUringFileWriter {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        // Flush any partial buffer (current write target)
        let state = self.ring.context_mut();
        if let Some(cursor) = state.buffers.front() {
            if cursor.position() > 0 {
                let cursor = state.buffers.pop_front().unwrap();
                self.internal_flush(cursor)?;
            }
        }

        // For SeekFrom::End, we need to wait for all pending writes to complete
        // so that file.metadata().len() returns the correct file size
        if matches!(pos, io::SeekFrom::End(_)) {
            self.drain()?;
        }

        let state = self.ring.context_mut();
        let old_offset = state.offset;
        match pos {
            io::SeekFrom::Start(new_offset) => state.offset = new_offset,
            io::SeekFrom::End(end_offset) => {
                state.offset = (state.file.metadata()?.len() as i64 + end_offset) as u64
            }
            io::SeekFrom::Current(offset_delta) => {
                state.offset = (state.offset as i64 + offset_delta) as u64
            }
        };
        Ok(old_offset)
    }
}

/// Holds the state of the writer.
struct IoUringFileWriterState {
    file: File,
    direct_io_file: Option<File>,
    offset: FileSize,
    buffers: VecDeque<Cursor<IoBufferChunk>>,
    stats: FileWriterStats,
}

impl IoUringFileWriterState {
    fn new(
        path: impl AsRef<Path>,
        buffers: VecDeque<Cursor<IoBufferChunk>>,
        use_direct_io: bool,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOATIME)
            .open(&path)?;
        let direct_io_file = if use_direct_io {
            Some(
                OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NOATIME | libc::O_DIRECT)
                    .open(path)?,
            )
        } else {
            None
        };
        Ok(Self {
            file,
            direct_io_file,
            offset: 0,
            buffers,
            stats: FileWriterStats::default(),
        })
    }

    fn mark_write_completed(&mut self, buf: IoBufferChunk) {
        // Push to back to avoid interrupting the buffer currently being filled at the front
        self.buffers.push_back(Cursor::new(buf));
    }

    fn log_stats(&self) {
        self.stats.log();
    }
}

#[derive(Debug, Default)]
struct FileWriterStats {
    direct_io_write_count: usize,
    regular_io_write_count: usize,
    num_buffers_free_total: usize,
    num_writes_total: usize,
}

impl FileWriterStats {
    fn log(&self) {
        let avg_num_buffers_free_during_write = if self.num_writes_total == 0 {
            0.0
        } else {
            self.num_buffers_free_total as f64 / self.num_writes_total as f64
        };
        log::info!(
            "files writer stats - num_direct_io_writes: {} num_regular_io_writes: {} \
             avg_num_buffers_free_during_write: {}",
            self.direct_io_write_count,
            self.regular_io_write_count,
            avg_num_buffers_free_during_write
        );
    }
}

#[derive(Debug)]
struct WriteOp {
    fd: types::Fd,
    offset: FileSize,
    buf: IoBufferChunk,
    buf_offset: IoSize,
    write_len: IoSize,
}

impl RingOp<IoUringFileWriterState> for WriteOp {
    fn entry(&mut self) -> squeue::Entry {
        let WriteOp {
            fd,
            offset,
            buf,
            buf_offset,
            write_len,
        } = self;

        // Safety: buf is owned by `WriteOp` during the operation handling by the kernel and
        // reclaimed after completion passed in a call to `mark_write_completed`.
        let buf_ptr = unsafe { buf.as_mut_ptr().byte_add(*buf_offset as usize) };
        let write_len = *write_len;

        let entry = match buf.io_buf_index() {
            Some(io_buf_index) => opcode::WriteFixed::new(*fd, buf_ptr, write_len, io_buf_index)
                .offset(*offset)
                .ioprio(IO_PRIO_BE_HIGHEST)
                .build(),
            None => opcode::Write::new(*fd, buf_ptr, write_len)
                .offset(*offset)
                .ioprio(IO_PRIO_BE_HIGHEST)
                .build(),
        };
        entry.flags(squeue::Flags::ASYNC)
    }

    fn complete(
        &mut self,
        ring: &mut Completion<'_, IoUringFileWriterState, WriteOp>,
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
            offset,
            buf,
            buf_offset,
            write_len,
            ..
        } = self;

        let buf = mem::replace(buf, IoBufferChunk::empty());
        let total_written = *buf_offset + written;

        if written < *write_len {
            log::warn!("short write ({written}/{})", *write_len);
            // partial writes always use regular file without direct io
            ring.push(WriteOp {
                fd: io_uring::types::Fd(ring.context().file.as_raw_fd()),
                offset: *offset + written as FileSize,
                buf,
                buf_offset: total_written,
                write_len: *write_len - written,
            });
            return Ok(());
        }

        ring.context_mut().mark_write_completed(buf);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::io::{Read, Write},
        tempfile::NamedTempFile,
    };
    fn check_writing_file(
        file_size: FileSize,
        backing_buffer_size: usize,
        write_size: IoSize,
        use_direct_io: bool,
    ) {
        let pattern: Vec<u8> = (0..251).collect();
        // Create a temp file and write the pattern to it repeatedly
        let mut temp_file = NamedTempFile::new().unwrap();

        let buf = PageAlignedMemory::new(backing_buffer_size).unwrap();
        let mut writer = IoUringFileWriterBuilder::new()
            .use_direct_io(use_direct_io)
            .write_size(write_size)
            .build_with_buffer(temp_file.path(), buf)
            .unwrap();
        for _ in 0..file_size as usize / pattern.len() {
            writer.write(&pattern).unwrap();
        }
        writer
            .write(&pattern[..file_size as usize % pattern.len()])
            .unwrap();
        writer.flush().unwrap();

        // Read contents and verify length
        let mut all_read_data = Vec::new();
        temp_file.read_to_end(&mut all_read_data).unwrap();
        assert_eq!(all_read_data.len() as FileSize, file_size);

        // Verify the contents
        for (i, byte) in all_read_data.iter().enumerate() {
            assert_eq!(*byte, pattern[i % pattern.len()], "Mismatch - pos {i}");
        }
    }

    /// Test with buffer larger than the whole file
    #[test]
    fn test_writing_small_file() {
        check_writing_file(2500, 4096, 1024, false);
        check_writing_file(2500, 4096, 2048, false);
        check_writing_file(2500, 4096, 4096, false);
    }

    /// Test with buffer smaller than the whole file
    #[test]
    fn test_writing_file_in_chunks() {
        check_writing_file(25_000, 16384, 1024, false);
        check_writing_file(25_000, 4096, 1024, false);
        check_writing_file(25_000, 4096, 2048, false);
        check_writing_file(25_000, 4096, 4096, false);
    }

    /// Test with buffer much smaller than the whole file
    #[test]
    fn test_writing_large_file() {
        check_writing_file(250_000, 32768, 1024, false);
        check_writing_file(250_000, 16384, 1024, false);
        check_writing_file(250_000, 4096, 1024, false);
        check_writing_file(250_000, 4096, 2048, false);
        check_writing_file(250_000, 4096, 4096, false);
    }

    #[test]
    fn test_direct_io_write() {
        check_writing_file(2_500, 4096, 4096, true);
        check_writing_file(2_500, 16384, 4096, true);
        check_writing_file(25_000, 4096, 4096, true);
        check_writing_file(25_000, 16384, 4096, true);
        check_writing_file(250_000, 4096, 4096, true);
        check_writing_file(250_000, 16384, 4096, true);
        check_writing_file(4096, 4096, 4096, true);
        check_writing_file(4096, 16384, 4096, true);
        check_writing_file(16384, 4096, 4096, true);
        check_writing_file(16384, 16384, 4096, true);
    }

    #[test]
    fn test_seek_from_start() {
        use std::io::Seek;

        let mut temp_file = NamedTempFile::new().unwrap();
        let buf = PageAlignedMemory::new(4096).unwrap();
        let mut writer = IoUringFileWriterBuilder::new()
            .write_size(1024)
            .build_with_buffer(temp_file.path(), buf)
            .unwrap();

        // Write some data
        writer.write_all(b"hello").unwrap();

        // Seek to start and overwrite
        let old_pos = writer.seek(io::SeekFrom::Start(0)).unwrap();
        assert_eq!(old_pos, 5); // was at position 5 after writing "hello"

        writer.write_all(b"HELLO").unwrap();
        writer.flush().unwrap();

        // Verify the file contains "HELLO"
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "HELLO");
    }

    #[test]
    fn test_seek_from_current() {
        use std::io::Seek;

        let mut temp_file = NamedTempFile::new().unwrap();
        let buf = PageAlignedMemory::new(4096).unwrap();
        let mut writer = IoUringFileWriterBuilder::new()
            .write_size(1024)
            .build_with_buffer(temp_file.path(), buf)
            .unwrap();

        // Write initial data
        writer.write_all(b"0123456789").unwrap();

        // Seek back 5 bytes from current position
        let old_pos = writer.seek(io::SeekFrom::Current(-5)).unwrap();
        assert_eq!(old_pos, 10);

        // Overwrite last 5 bytes
        writer.write_all(b"ABCDE").unwrap();
        writer.flush().unwrap();

        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "01234ABCDE");
    }

    #[test]
    fn test_seek_from_end() {
        use std::io::Seek;

        let mut temp_file = NamedTempFile::new().unwrap();
        let buf = PageAlignedMemory::new(4096).unwrap();
        let mut writer = IoUringFileWriterBuilder::new()
            .write_size(1024)
            .build_with_buffer(temp_file.path(), buf)
            .unwrap();

        // Write initial data
        writer.write_all(b"hello world").unwrap();

        // Seek to 5 bytes before end
        let old_pos = writer.seek(io::SeekFrom::End(-5)).unwrap();
        assert_eq!(old_pos, 11);

        // Overwrite "world" with "WORLD"
        writer.write_all(b"WORLD").unwrap();
        writer.flush().unwrap();

        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "hello WORLD");
    }

    #[test]
    fn test_seek_flushes_partial_buffer() {
        use std::io::Seek;

        let mut temp_file = NamedTempFile::new().unwrap();
        let buf = PageAlignedMemory::new(4096).unwrap();
        let mut writer = IoUringFileWriterBuilder::new()
            .write_size(1024)
            .build_with_buffer(temp_file.path(), buf)
            .unwrap();

        // Write less than buffer size (should stay in buffer)
        writer.write_all(b"partial").unwrap();

        // Seek should flush the partial buffer
        writer.seek(io::SeekFrom::Start(0)).unwrap();

        // Write at the start
        writer.write_all(b"PARTIAL").unwrap();
        writer.flush().unwrap();

        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "PARTIAL");
    }

    #[test]
    fn test_seek_and_extend_file() {
        use std::io::Seek;

        let mut temp_file = NamedTempFile::new().unwrap();
        let buf = PageAlignedMemory::new(4096).unwrap();
        let mut writer = IoUringFileWriterBuilder::new()
            .write_size(1024)
            .build_with_buffer(temp_file.path(), buf)
            .unwrap();

        // Write initial data
        writer.write_all(b"start").unwrap();
        writer.flush().unwrap();

        // Seek past end and write (creates a hole)
        writer.seek(io::SeekFrom::Start(10)).unwrap();
        writer.write_all(b"end").unwrap();
        writer.flush().unwrap();

        let mut contents = vec![];
        temp_file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents.len(), 13);
        assert_eq!(&contents[0..5], b"start");
        assert_eq!(&contents[10..13], b"end");
        // Bytes 5-9 are undefined (sparse or zeros depending on filesystem)
    }
}
