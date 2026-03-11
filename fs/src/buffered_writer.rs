use {
    crate::FileSize,
    std::{
        fs,
        io::{self, BufWriter},
        path::Path,
    },
};

/// Default buffer size for writing large files to disks. Since current implementation does not do
/// background writing, this size is set above minimum reasonable SSD write sizes to also reduce
/// number of syscalls.
const DEFAULT_BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Return a buffered writer for creating a new file at `path`
///
/// The returned writer is using a buffer size tuned for writing large files to disks.
pub fn large_file_buf_writer(path: impl AsRef<Path>) -> io::Result<impl io::Write> {
    let file = fs::File::create(path)?;

    Ok(BufWriter::with_capacity(DEFAULT_BUFFER_SIZE, file))
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
