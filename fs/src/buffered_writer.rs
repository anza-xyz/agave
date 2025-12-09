use std::{
    fs,
    io::{self, BufWriter},
    path::Path,
};

/// Default buffer size for writing large files to disks. Since current implementation does not do
/// background writing, this size is set above minimum reasonable SSD write sizes to also reduce
/// number of syscalls.
const DEFAULT_BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Return a buffered writer for creating a new file at `path`
///
/// The returned writer is using a buffer size tuned for writing large files to disks.
pub fn large_file_buf_writer(path: impl AsRef<Path>) -> io::Result<impl io::Write + io::Seek> {
    let file = fs::File::create(path)?;

    Ok(BufWriter::with_capacity(DEFAULT_BUFFER_SIZE, file))
}
