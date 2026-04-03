use {
    crate::{
        FileSize, IoSize,
        file_io::FileCreator,
        io_uring::{file_creator::IoUringFileCreator, memory::IoBufferChunk},
    },
    std::{
        fs::File,
        io::{self, Result, Write},
        path::PathBuf,
        sync::Arc,
    },
};

pub struct IoUringFileWriter<'a> {
    /// Write buffer, acquired on demand during writes.
    /// `None` initially and after each full buffer is submitted to io_uring.
    ///
    /// # Safety
    /// `IoBufferChunk` borrows memory owned by `inner`. This is safe because Rust drops
    /// fields in declaration order, so `current_buffer` is always dropped before `inner`.
    current_buffer: Option<IoBufferChunk>,
    inner: IoUringFileCreator<'a>,
    current_buffer_offset: IoSize,
    file_offset: FileSize,
    file_key: usize,
    finalized: bool,
}

impl<'a> IoUringFileWriter<'a> {
    pub fn new(
        mut file_creator: IoUringFileCreator<'a>,
        path: PathBuf,
        mode: u32,
        dir_handle: Arc<File>,
    ) -> Result<Self> {
        let file_key = file_creator.open(path, mode, dir_handle)?;
        Ok(Self {
            inner: file_creator,
            current_buffer: None,
            current_buffer_offset: 0,
            file_key,
            file_offset: 0,
            finalized: false,
        })
    }

    /// Writes the current buffer and sets `self.current_buffer` to `None`
    fn write_current_buffer(&mut self, is_final_write: bool) -> Result<()> {
        debug_assert!(self.current_buffer.is_some());

        if let Some(current_buffer) = self.current_buffer.take() {
            self.inner.schedule_write(
                self.file_key,
                self.file_offset,
                is_final_write,
                current_buffer,
                self.current_buffer_offset,
            )?;
            self.file_offset = self
                .file_offset
                .strict_add(self.current_buffer_offset as FileSize);
            self.current_buffer_offset = 0;
        }
        Ok(())
    }
}

impl Write for IoUringFileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        // @TODO -- once file creator supports multiple partial writes we
        // can support multiple flushes
        if self.finalized {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file writer has already been finalized by flush",
            ));
        }

        // Get a new buffer if we need to
        let current_buffer = if let Some(buf) = self.current_buffer.as_mut() {
            buf
        } else {
            self.current_buffer.insert(self.inner.wait_free_buf()?)
        };

        let remaining_buf_len =
            current_buffer.len().strict_sub(self.current_buffer_offset) as usize;
        if remaining_buf_len <= buf.len() {
            // Copy as many bytes as we can into the buffer and then send it to the io uring
            current_buffer.copy_from_slice(&buf[..remaining_buf_len], self.current_buffer_offset);
            self.current_buffer_offset = current_buffer.len();
            self.write_current_buffer(false)?;
            Ok(remaining_buf_len)
        } else {
            current_buffer.copy_from_slice(buf, self.current_buffer_offset);
            self.current_buffer_offset = self.current_buffer_offset.strict_add(buf.len() as IoSize);
            Ok(buf.len())
        }
    }

    /// Flush can only be called once.
    /// Flush will close the underlying file.
    fn flush(&mut self) -> Result<()> {
        // @TODO -- once file creator supports multiple partial writes we
        // can support multiple flushes
        if self.finalized {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file writer has already been finalized by flush",
            ));
        }

        // Handle the case where we are flushing after the buffer has already been dispatched during a write.
        // This would mean that `self.current_buffer = None`, but we still need to fetch a new buffer
        // just to do a zero byte write that will close the file.
        if self.current_buffer.is_none() {
            debug_assert_eq!(self.current_buffer_offset, 0);
            self.current_buffer = Some(self.inner.wait_free_buf()?);
        }

        // Flush the current buffer
        self.finalized = true;
        self.write_current_buffer(true)?;

        // Wait for all the fs ops to drain and then return
        self.inner.drain()
    }
}

impl Drop for IoUringFileWriter<'_> {
    fn drop(&mut self) {
        if !self.finalized {
            let flush_res = self.flush();
            debug_assert!(flush_res.is_ok());
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            FileSize, IoSize,
            io_uring::{
                file_creator::{DIRECT_IO_WRITE_LEN_ALIGNMENT, IoUringFileCreatorBuilder},
                file_writer::IoUringFileWriter,
            },
        },
        std::{
            fs::File,
            io::{Read, Write},
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
        },
        test_case::test_case,
    };

    // Check several edge cases:
    // * creating empty file
    // * file content is a multiple of write size
    // * buffer holds single write size (1 internal buffer is used)
    // * negations and combinations of above
    #[test_case(0, 2u32.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT), DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(DIRECT_IO_WRITE_LEN_ALIGNMENT as u64, DIRECT_IO_WRITE_LEN_ALIGNMENT, DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(2u64.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT as u64), DIRECT_IO_WRITE_LEN_ALIGNMENT, DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(4u64.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT as u64), 2u32.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT), DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(9u64.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT as u64), DIRECT_IO_WRITE_LEN_ALIGNMENT, DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(9u64.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT as u64), 2u32.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT), DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case(2u64.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT as u64).strict_add(1), 2u32.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT), DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    #[test_case((DIRECT_IO_WRITE_LEN_ALIGNMENT as u64).strict_sub(1), 2u32.strict_mul(DIRECT_IO_WRITE_LEN_ALIGNMENT), DIRECT_IO_WRITE_LEN_ALIGNMENT)]
    fn test_write_chunked_content(file_size: FileSize, buf_size: IoSize, write_size: IoSize) {
        // test with and without direct io
        write_and_verify(file_size, buf_size, write_size, true);
        write_and_verify(file_size, buf_size, write_size, false);
    }

    fn write_and_verify(
        file_size: FileSize,
        buf_size: IoSize,
        write_size: IoSize,
        use_direct_io: bool,
    ) {
        let original_contents = (0..file_size).map(|i| i as u8).collect::<Vec<_>>();
        let (contents_a, contents_b) = original_contents.split_at(file_size as usize / 3);
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = Arc::new(File::open(temp_dir.path()).unwrap());
        let file_path = temp_dir.path().join("test.txt");
        let file_closed = Arc::new(AtomicBool::new(false));
        let io_uring_file_creator = IoUringFileCreatorBuilder::new()
            .write_with_direct_io(use_direct_io)
            .write_capacity(write_size)
            .build(buf_size as usize, |file_info| {
                assert_eq!(file_info.size, original_contents.len() as FileSize);
                file_closed.store(true, Ordering::Relaxed);
                Some(file_info.file)
            })
            .unwrap();

        let mut io_uring_file_writer =
            IoUringFileWriter::new(io_uring_file_creator, file_path.clone(), 0o666, dir).unwrap();

        io_uring_file_writer.write_all(contents_a).unwrap();
        io_uring_file_writer.write_all(contents_b).unwrap();
        io_uring_file_writer.flush().unwrap();
        drop(io_uring_file_writer);
        assert!(file_closed.load(Ordering::Relaxed));

        let mut current_contents = Vec::new();
        File::open(&file_path)
            .unwrap()
            .read_to_end(&mut current_contents)
            .unwrap();
        assert_eq!(current_contents, original_contents,);
    }

    #[test]
    fn test_allow_only_one_flush() {
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = Arc::new(File::open(temp_dir.path()).unwrap());
        let file_path = temp_dir.path().join("test.txt");
        let io_uring_file_creator = IoUringFileCreatorBuilder::new()
            .build(4096, |_| None)
            .unwrap();

        let mut io_uring_file_writer =
            IoUringFileWriter::new(io_uring_file_creator, file_path.clone(), 0o666, dir).unwrap();
        io_uring_file_writer.flush().unwrap();
        assert!(io_uring_file_writer.write(&[0]).is_err());
        assert!(io_uring_file_writer.flush().is_err());
    }
}
