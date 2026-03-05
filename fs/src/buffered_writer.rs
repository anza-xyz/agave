use std::{
    fs,
    io::{self, BufWriter},
    path::{Path, PathBuf},
};

/// Default buffer size for writing large files to disks. Since current implementation does not do
/// background writing, this size is set above minimum reasonable SSD write sizes to also reduce
/// number of syscalls.
const DEFAULT_BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Return a buffered writer for creating a new file at `path`
///
/// The returned writer is using a buffer size tuned for writing large files to disks.
pub fn large_file_buf_writer(path: &Path) -> io::Result<impl io::Write + io::Seek> {
    let file = fs::File::create(path)?;

    Ok(BufWriter::with_capacity(DEFAULT_BUFFER_SIZE, file))
}

/// Atomically populates a target path by first writing content using a temporary path.
///
/// Takes a target path and a function that creates an inner `W` object to populate it. The
/// function receives a temporary path (`<target>.tmp`) instead of the target directly; on success
/// the temporary file is atomically renamed to the target path.
///
/// When `W` implements [`io::Write`], call [`finalize`](Self::finalize) to flush the writer and
/// propagate any rename errors. If dropped without calling `finalize`, the same rename logic runs
/// but without flushing the inner writer or reporting errors. On any error the temporary file is
/// left on disk for inspection.
pub struct AtomicPathFinalizer<W> {
    inner: Option<W>,
    temp_path: PathBuf,
    target_path: PathBuf,
    has_error: bool,
}

impl<W> AtomicPathFinalizer<W> {
    /// Create a new instance targeting `path` using `W` utility obtained from `make_inner`.
    ///
    /// `make_inner` is called with a temporary path and must return a `W` that will populate
    /// that over the object's lifetime.
    pub fn new(
        path: impl AsRef<Path>,
        make_inner: impl FnOnce(&Path) -> io::Result<W>,
    ) -> io::Result<Self> {
        let target_path = path.as_ref().to_path_buf();
        let mut temp_path = target_path.clone();
        temp_path.add_extension("tmp");
        Ok(Self {
            inner: Some(make_inner(&temp_path)?),
            temp_path,
            target_path,
            has_error: false,
        })
    }

    fn drop_and_rename(&mut self) -> io::Result<()> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };
        if self.has_error {
            return Err(io::Error::other("prior error occurred, can't finalize"));
        }
        drop(inner);
        fs::rename(&self.temp_path, &self.target_path)
    }

    #[inline]
    fn with_inner<T>(&mut self, f: impl FnOnce(&mut W) -> io::Result<T>) -> io::Result<T> {
        let inner = self.inner.as_mut().expect("must not be called after drop");
        f(inner).inspect_err(|_| self.has_error = true)
    }
}

impl<W: io::Write> AtomicPathFinalizer<W> {
    /// Flush the inner writer and rename the temporary file to the target path.
    ///
    /// Returns an error if a prior write error occurred, if flushing fails, or if the rename
    /// fails.
    pub fn finalize(mut self) -> io::Result<()> {
        io::Write::flush(&mut self)?;
        self.drop_and_rename()
    }
}

impl<W> Drop for AtomicPathFinalizer<W> {
    fn drop(&mut self) {
        let _ = self.drop_and_rename();
    }
}

impl<W: io::Write> io::Write for AtomicPathFinalizer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.with_inner(|w| w.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.with_inner(|w| w.flush())
    }
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.with_inner(|w| w.write_vectored(bufs))
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.with_inner(|w| w.write_all(buf))
    }
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        self.with_inner(|w| w.write_fmt(args))
    }
}

impl<W: io::Seek> io::Seek for AtomicPathFinalizer<W> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.with_inner(|w| w.seek(pos))
    }
    fn rewind(&mut self) -> io::Result<()> {
        self.with_inner(|w| w.rewind())
    }
    fn stream_position(&mut self) -> io::Result<u64> {
        self.with_inner(|w| w.stream_position())
    }
    fn seek_relative(&mut self, offset: i64) -> io::Result<()> {
        self.with_inner(|w| w.seek_relative(offset))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Write as _, tempfile::TempDir};

    #[test]
    fn test_path_finalizer_uses_tmp_during_write() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("output");

        let mut w = AtomicPathFinalizer::new(&target, large_file_buf_writer).unwrap();
        write!(w, "hello").unwrap();

        assert!(!target.exists());
        assert!(target.with_extension("tmp").exists());

        w.finalize().unwrap();

        assert!(target.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        assert!(!target.with_extension("tmp").exists());
    }

    #[test]
    fn test_path_finalizer_write_error_leaves_temp() {
        struct ErrorWriter(fs::File);
        impl io::Write for ErrorWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write error"))
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }

        let dir = TempDir::new().unwrap();
        let target = dir.path().join("output.txt");
        {
            let mut w =
                AtomicPathFinalizer::new(&target, |path| fs::File::create(path).map(ErrorWriter))
                    .unwrap();
            assert!(write!(w, "data").is_err());
            assert!(w.finalize().is_err());
        }

        assert!(!target.exists());
        assert!(dir.path().join("output.txt.tmp").exists());
    }
}
