use {
    crate::{
        FileSize, IoSize,
        file_io::FileCreator,
        io_setup::IoSetupState,
        io_uring::{file_creator::IoUringFileCreator, memory::IoBufferChunk},
    },
    std::{
        fs,
        io::{self, BufWriter, Result},
        path::PathBuf,
        sync::Arc,
    },
};

/// Default buffer size for writing large files to disks. Since current implementation does not do
/// background writing, this size is set above minimum reasonable SSD write sizes to also reduce
/// number of syscalls.
const DEFAULT_BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Return a buffered writer for creating a new file at `path`
///
/// The returned writer is using a buffer size tuned for writing large files to disks.
pub fn large_file_buf_writer(
    path: PathBuf,
    io_setup: &IoSetupState,
) -> io::Result<Box<dyn io::Write>> {
    #[cfg(target_os = "linux")]
    if agave_io_uring::io_uring_supported() {
        use crate::io_uring::file_creator::{DEFAULT_WRITE_SIZE, IoUringFileCreatorBuilder};

        let mut io_uring_file_creator = IoUringFileCreatorBuilder::new()
            .use_registered_buffers(io_setup.use_registered_io_uring_buffers)
            .write_with_direct_io(io_setup.use_direct_io)
            .shared_sqpoll(io_setup.shared_sqpoll_fd())
            .build((DEFAULT_WRITE_SIZE as usize).strict_mul(4), |_| None)?;
        let parent_dir_handle = Arc::new(fs::File::open(path.parent().ok_or(io::Error::new(
            io::ErrorKind::NotADirectory,
            "large file buf writer expected the file path to have a parent directory",
        ))?)?);
        let file_key = io_uring_file_creator.open(path, 0o644, parent_dir_handle)?;

        return Ok(Box::new(IoUringFileWriter::new(
            io_uring_file_creator,
            file_key,
        )));
    }

    let file = fs::File::create(path)?;
    Ok(Box::new(BufWriter::with_capacity(
        DEFAULT_BUFFER_SIZE,
        file,
    )))
}

/// A writer that enforces a hard byte-count limit on the wrapped writer.
///
/// Each call to [`write`](io::Write::write) checks whether the buffer fits within the remaining
/// quota. If it does not, the write fails immediately with [`io::ErrorKind::FileTooLarge`]
/// before any bytes are forwarded to the inner writer.
pub struct SizeLimitedWriter<W: io::Write> {
    inner: W,
    limit: FileSize,
    bytes_written: FileSize,
}

impl<W: io::Write> SizeLimitedWriter<W> {
    pub fn new(inner: W, limit: FileSize) -> Self {
        Self {
            inner,
            limit,
            bytes_written: 0,
        }
    }

    /// Returns the number of bytes successfully written so far.
    pub fn bytes_written(&self) -> FileSize {
        self.bytes_written
    }
}

impl<W: io::Write> io::Write for SizeLimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let attempted_size = self.bytes_written.checked_add(buf.len() as FileSize);
        if attempted_size.is_none_or(|size| size > self.limit) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "write of {} bytes would exceed limit of {} ({} already written)",
                    buf.len(),
                    self.limit,
                    self.bytes_written,
                ),
            ));
        }
        let n = self.inner.write(buf)?;
        // `n` should never be > buf.len() per `write` contract and overflow was already checked above
        self.bytes_written = self.bytes_written.wrapping_add(n as FileSize);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct IoUringFileWriter<'a> {
    inner: IoUringFileCreator<'a>,
    // Safety: the IoBufferChunk is safe to use as long as the inner IoUringFileCreator is still alive
    current_buffer: Option<IoBufferChunk>,
    current_buffer_offset: IoSize,
    file_offset: u64,
    file_key: usize,
}

impl<'a> IoUringFileWriter<'a> {
    pub fn new(file_creator: IoUringFileCreator<'a>, file_key: usize) -> Self {
        Self {
            inner: file_creator,
            current_buffer: None,
            current_buffer_offset: 0,
            file_key,
            file_offset: 0,
        }
    }

    fn write_current_buffer(&mut self, is_final_write: bool) -> Result<()> {
        // Replace `self.current_buffer` with None. The next write will have to wait for a free buffer.
        if let Some(buf) = self.current_buffer.take() {
            self.inner.schedule_write(
                self.file_key,
                self.file_offset,
                is_final_write,
                buf,
                self.current_buffer_offset,
            )?;
            self.file_offset = self
                .file_offset
                .strict_add(self.current_buffer_offset as u64);
            self.current_buffer_offset = 0;
        }

        Ok(())
    }
}

impl io::Write for IoUringFileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        // buffer was filled and queued to write, fetch a new buffer
        if self.current_buffer.is_none() {
            self.current_buffer_offset = 0;
            self.current_buffer = Some(self.inner.wait_free_buf()?);
        }

        // unwrap the buffer. we just ensured that it is not `None`
        let current_buffer = self.current_buffer.as_ref().unwrap();
        let mut dst = unsafe {
            std::slice::from_raw_parts_mut(
                current_buffer.as_mut_ptr(),
                current_buffer.len() as usize,
            )
        };
        dst = &mut dst[self.current_buffer_offset as usize..];

        // copy as many bytes as we can into the buffer
        let bytes_copied =
            (current_buffer.len().strict_sub(self.current_buffer_offset)).min(buf.len() as u32);
        dst.copy_from_slice(&buf[..bytes_copied as usize]);
        self.current_buffer_offset = self
            .current_buffer_offset
            .strict_add(bytes_copied as IoSize);

        // check if the buffer is full. if it is, schedule a write
        if self.current_buffer_offset == current_buffer.len() {
            self.write_current_buffer(false)?;
        }

        Ok(bytes_copied as usize)
    }

    /// Flush is only expected to be called once.
    fn flush(&mut self) -> Result<()> {
        // flush the current buffer if necessary
        if self.current_buffer_offset != 0 {
            self.write_current_buffer(true)?;
        }
        // wait for all the fs ops to drain and then return
        self.inner.drain()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, io::Write as _, test_case::test_case};

    #[test_case(10, b"hello" ; "within limit")]
    #[test_case(5,  b"hello" ; "exactly at limit")]
    fn size_limited_writer_accepts(limit: FileSize, data: &[u8]) {
        let mut buf = Vec::new();
        let mut w = SizeLimitedWriter::new(&mut buf, limit);
        w.write_all(data).unwrap();
        assert_eq!(w.bytes_written(), data.len() as FileSize);
        assert_eq!(buf, data);
    }

    #[test_case(4, b"hello" ; "exceeds limit")]
    #[test_case(0, b"x"     ; "zero limit")]
    fn size_limited_writer_rejects(limit: FileSize, data: &[u8]) {
        let mut buf = Vec::new();
        let mut w = SizeLimitedWriter::new(&mut buf, limit);
        let err = w.write(data).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(w.bytes_written(), 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn size_limited_writer_rejects_on_cumulative_overflow() {
        let mut buf = Vec::new();
        let mut w = SizeLimitedWriter::new(&mut buf, 5);
        w.write_all(b"hi").unwrap();
        // 3 bytes remain; writing 4 must fail
        let err = w.write(b"four").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(w.bytes_written(), 2);
        assert_eq!(buf, b"hi");
    }
}
