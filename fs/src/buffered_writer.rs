use {
    crate::io_uring::file_writer::IoUringFileWriterBuilder,
    std::{
        fs,
        io::{self, BufWriter},
        path::Path,
    },
};

/// Default buffer size for writing large files to disks. Since current implementation does not do
/// background writing, this size is set above minimum reasonable SSD write sizes to also reduce
/// number of syscalls.
const DEFAULT_BUFFER_SIZE: usize = 4 * crate::io_uring::file_writer::DEFAULT_WRITE_SIZE as usize;

/// Return a buffered writer for creating a new file at `path`
///
/// The returned writer is using a buffer size tuned for writing large files to disks.
pub fn large_file_buf_writer(path: impl AsRef<Path>) -> io::Result<impl io::Write + io::Seek> {
    fs::File::create(&path)?;

    // IoUringFileWriter has poor perf on small writes even when it's just copying to its internal buffer
    // putting a bufwriter around it makes it significantly faster for small writes
    Ok(BufWriter::new(
        IoUringFileWriterBuilder::new().build(path, DEFAULT_BUFFER_SIZE)?,
    ))
}
