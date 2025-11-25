#![allow(clippy::arithmetic_side_effects)]

//! File i/o helper functions.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

/// `buffer` contains `valid_bytes` of data at its end.
/// Move those valid bytes to the beginning of `buffer`, then read from `offset` to fill the rest of `buffer`.
/// Update `offset` for the next read and update `valid_bytes` to specify valid portion of `buffer`.
/// `valid_file_len` is # of valid bytes in the file. This may be <= file length.
pub fn read_more_buffer(
    file: &File,
    valid_file_len: usize,
    offset: &mut usize,
    buffer: &mut [u8],
    valid_bytes: &mut Range<usize>,
) -> std::io::Result<()> {
    // copy remainder of `valid_bytes` into beginning of `buffer`.
    // These bytes were left over from the last read but weren't enough for the next desired contiguous read chunk.
    buffer.copy_within(valid_bytes.clone(), 0);

    // read the rest of `buffer`
    let bytes_read = read_into_buffer(
        file,
        valid_file_len,
        *offset,
        &mut buffer[valid_bytes.len()..],
    )?;
    *offset += bytes_read;
    *valid_bytes = 0..(valid_bytes.len() + bytes_read);

    Ok(())
}

#[cfg(unix)]
fn arch_read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::prelude::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn arch_read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    // Note: as opposed to unix `read_at` this call will update the internal file offset,
    // so all callers should consistently use the file only through this module
    file.seek_read(buffer, offset)
}

#[cfg(unix)]
fn arch_write_at(file: &File, buffer: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::prelude::FileExt;
    file.write_at(buffer, offset)
}

#[cfg(windows)]
fn arch_write_at(file: &File, buffer: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    // Note: as opposed to unix `write_at` this call will update the internal file offset,
    // so all callers should consistently use the file only through this module
    file.seek_write(buffer, offset)
}

/// Write, starting at `offset`, the whole buffer to a file irrespective of the file's current length.
///
/// After this operation file size may be extended and the file cursor may be moved (platform-dependent).
pub fn write_buffer_to_file(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let wrote_len = arch_write_at(file, buffer, offset)?;
        if wrote_len == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buffer = &buffer[wrote_len..];
        offset += wrote_len as u64;
    }
    Ok(())
}

/// Read, starting at `start_offset`, until `buffer` is full or we read past `valid_file_len`/eof.
/// `valid_file_len` is # of valid bytes in the file. This may be <= file length.
/// return # bytes read
pub fn read_into_buffer(
    file: &File,
    valid_file_len: usize,
    start_offset: usize,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    let mut offset = start_offset;
    let mut buffer_offset = 0;
    let mut total_bytes_read = 0;
    if start_offset >= valid_file_len {
        return Ok(0);
    }

    while buffer_offset < buffer.len() {
        match arch_read_at(file, &mut buffer[buffer_offset..], offset as u64) {
            Err(err) => {
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            Ok(bytes_read_this_time) => {
                total_bytes_read += bytes_read_this_time;
                if total_bytes_read + start_offset >= valid_file_len {
                    total_bytes_read -= (total_bytes_read + start_offset) - valid_file_len;
                    // we've read all there is in the file
                    break;
                }
                // There is possibly more to read. `read_at` may have returned partial results, so prepare to loop and read again.
                buffer_offset += bytes_read_this_time;
                offset += bytes_read_this_time;
            }
        }
    }
    Ok(total_bytes_read)
}

/// An asynchronous queue for file creation.
pub trait FileCreator {
    /// Schedule creating a file at `path` with `mode` permissions and bytes read from `contents`.
    ///
    /// `parent_dir_handle` is assumed to be a parent directory of `path` such that file may be
    /// created using optimized kernel API to create `path.file_name()` inside `parent_dir_handle`.
    fn schedule_create_at_dir(
        &mut self,
        path: PathBuf,
        mode: u32,
        parent_dir_handle: Arc<File>,
        contents: &mut dyn io::Read,
    ) -> io::Result<()>;

    /// Invoke implementation specific logic to handle file creation completion.
    fn file_complete(&mut self, path: PathBuf);

    /// Waits for all operations to be completed
    fn drain(&mut self) -> io::Result<()>;
}

pub fn file_creator<'a>(
    buf_size: usize,
    file_complete: impl FnMut(PathBuf) + 'a,
) -> io::Result<Box<dyn FileCreator + 'a>> {
    #[cfg(target_os = "linux")]
    if agave_io_uring::io_uring_supported() {
        use crate::io_uring::file_creator::{IoUringFileCreator, DEFAULT_WRITE_SIZE};

        if buf_size >= DEFAULT_WRITE_SIZE {
            let io_uring_creator =
                IoUringFileCreator::with_buffer_capacity(buf_size, file_complete)?;
            return Ok(Box::new(io_uring_creator));
        }
    }
    Ok(Box::new(SyncIoFileCreator::new(buf_size, file_complete)))
}

pub struct SyncIoFileCreator<'a> {
    file_complete: Box<dyn FnMut(PathBuf) + 'a>,
}

impl<'a> SyncIoFileCreator<'a> {
    fn new(_buf_size: usize, file_complete: impl FnMut(PathBuf) + 'a) -> Self {
        Self {
            file_complete: Box::new(file_complete),
        }
    }
}

/// Update permissions mode of an existing directory or file path
///
/// Note: on-non Unix platforms, this functions only updates readonly mode.
pub fn set_path_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let perm;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perm = fs::Permissions::from_mode(mode);
    }
    #[cfg(not(unix))]
    {
        let mut current_perm = fs::metadata(path)?.permissions();
        current_perm.set_readonly(mode & 0o200 == 0);
        perm = current_perm;
    }
    fs::set_permissions(path, perm)
}

impl FileCreator for SyncIoFileCreator<'_> {
    fn schedule_create_at_dir(
        &mut self,
        path: PathBuf,
        mode: u32,
        _parent_dir_handle: Arc<File>,
        contents: &mut dyn io::Read,
    ) -> io::Result<()> {
        // Open for writing (also allows overwrite) and apply `mode`
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);

        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);

        let mut file_buf = BufWriter::new(options.open(&path)?);
        io::copy(contents, &mut file_buf)?;
        file_buf.flush()?;

        // On unix the file was opened with proper permissions, only update the mode on non-unix
        #[cfg(not(unix))]
        set_path_permissions(&path, mode)?;

        self.file_complete(path);
        Ok(())
    }

    fn file_complete(&mut self, path: PathBuf) {
        (self.file_complete)(path)
    }

    fn drain(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            fs,
            io::{Cursor, Write},
        },
        tempfile::tempfile,
    };

    #[test]
    fn test_read_into_buffer() {
        // Setup a sample file with 32 bytes of data
        let mut sample_file = tempfile().unwrap();
        let file_size = 32;
        let bytes: Vec<u8> = (0..file_size as u8).collect();
        sample_file.write_all(&bytes).unwrap();

        // Read all 32 bytes into buffer
        let mut buffer = [0; 32];
        let mut buffer_len = buffer.len();
        let mut valid_len = 32;
        let mut start_offset = 0;
        let num_bytes_read =
            read_into_buffer(&sample_file, valid_len, start_offset, &mut buffer).unwrap();
        assert_eq!(num_bytes_read, buffer_len);
        assert_eq!(bytes, buffer);

        // Given a 64-byte buffer, it should only read 32 bytes into the buffer
        let mut buffer = [0; 64];
        buffer_len = buffer.len();
        let num_bytes_read =
            read_into_buffer(&sample_file, valid_len, start_offset, &mut buffer).unwrap();
        assert_eq!(num_bytes_read, valid_len);
        assert_eq!(bytes, buffer[0..valid_len]);
        assert_eq!(buffer[valid_len..buffer_len], [0; 32]);

        // Given the `valid_file_len` is 16, it should only read 16 bytes into the buffer
        let mut buffer = [0; 32];
        buffer_len = buffer.len();
        valid_len = 16;
        let num_bytes_read =
            read_into_buffer(&sample_file, valid_len, start_offset, &mut buffer).unwrap();
        assert_eq!(num_bytes_read, valid_len);
        assert_eq!(bytes[0..valid_len], buffer[0..valid_len]);
        // As a side effect of the `read_into_buffer` the data passed `valid_file_len` was
        // read and put into the buffer, though these data should not be
        // consumed.
        assert_eq!(buffer[valid_len..buffer_len], bytes[valid_len..buffer_len]);

        // Given the start offset 8, it should only read 24 bytes into buffer
        let mut buffer = [0; 32];
        buffer_len = buffer.len();
        valid_len = 32;
        start_offset = 8;
        let num_bytes_read =
            read_into_buffer(&sample_file, valid_len, start_offset, &mut buffer).unwrap();
        assert_eq!(num_bytes_read, valid_len - start_offset);
        assert_eq!(buffer[0..num_bytes_read], bytes[start_offset..buffer_len]);
        assert_eq!(buffer[num_bytes_read..buffer_len], [0; 8])
    }

    #[test]
    fn test_read_more_buffer() {
        // Setup a sample file with 32 bytes of data
        let mut sample_file = tempfile().unwrap();
        let file_size = 32;
        let bytes: Vec<u8> = (0..file_size as u8).collect();
        sample_file.write_all(&bytes).unwrap();

        // Should move left-over 8 bytes to and read 24 bytes from file
        let mut buffer = [0xFFu8; 32];
        let buffer_len = buffer.len();
        let mut offset = 0;
        let mut valid_bytes = 24..32;
        let mut valid_bytes_len = valid_bytes.len();
        let valid_len = 32;
        read_more_buffer(
            &sample_file,
            valid_len,
            &mut offset,
            &mut buffer,
            &mut valid_bytes,
        )
        .unwrap();
        assert_eq!(offset, buffer_len - valid_bytes_len);
        assert_eq!(valid_bytes, 0..buffer_len);
        assert_eq!(buffer[0..valid_bytes_len], [0xFFu8; 8]);
        assert_eq!(
            buffer[valid_bytes_len..buffer_len],
            bytes[0..buffer_len - valid_bytes_len]
        );

        // Should move left-over 8 bytes to and read 16 bytes from file due to EOF
        let mut buffer = [0xFFu8; 32];
        let start_offset = 16;
        let mut offset = 16;
        let mut valid_bytes = 24..32;
        valid_bytes_len = valid_bytes.len();
        read_more_buffer(
            &sample_file,
            valid_len,
            &mut offset,
            &mut buffer,
            &mut valid_bytes,
        )
        .unwrap();
        assert_eq!(offset, file_size);
        assert_eq!(valid_bytes, 0..24);
        assert_eq!(buffer[0..valid_bytes_len], [0xFFu8; 8]);
        assert_eq!(
            buffer[valid_bytes_len..valid_bytes.end],
            bytes[start_offset..file_size]
        );
    }

    fn read_file_to_string(path: &PathBuf) -> String {
        String::from_utf8(fs::read(path).expect("Failed to read file"))
            .expect("Failed to decode file contents")
    }

    #[test]
    fn test_create_writes_contents() -> io::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let file_path = temp_dir.path().join("test.txt");
        let contents = "Hello, world!";

        // Shared state to capture callback invocations
        let mut callback_invoked_path = None;

        // Instantiate FileCreator
        let mut creator = file_creator(2 << 20, |path| {
            callback_invoked_path.replace(path);
        })?;

        let dir = Arc::new(File::open(temp_dir.path())?);
        creator.schedule_create_at_dir(
            file_path.clone(),
            0o644,
            dir,
            &mut Cursor::new(contents),
        )?;
        creator.drain()?;
        drop(creator);

        assert_eq!(read_file_to_string(&file_path), contents);
        assert_eq!(callback_invoked_path, Some(file_path));

        Ok(())
    }

    #[test]
    fn test_multiple_file_creations() -> io::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let mut callback_counter = 0;

        let mut creator = file_creator(2 << 20, |path: PathBuf| {
            let contents = read_file_to_string(&path);
            assert!(contents.starts_with("File "));
            callback_counter += 1;
        })?;

        let dir = Arc::new(File::open(temp_dir.path())?);
        for i in 0..5 {
            let file_path = temp_dir.path().join(format!("file_{i}.txt"));
            let data = format!("File {i}");
            creator.schedule_create_at_dir(
                file_path,
                0o600,
                dir.clone(),
                &mut Cursor::new(data),
            )?;
        }
        creator.drain()?;
        drop(creator);

        assert_eq!(callback_counter, 5);
        Ok(())
    }
}
