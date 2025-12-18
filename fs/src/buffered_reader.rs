#![allow(clippy::arithmetic_side_effects)]

//! File I/O buffered readers for AppendVec
//! Specialized `BufRead`-like types for reading account data.
//!
//! Callers can use these types to iterate efficiently over append vecs. They can do so by repeatedly
//! calling:
//! * `fill_buf_required(account_meta_len)` to scan the account metadata parts and determine the account
//!   data size,
//!  * optionally extend the obtained buffer to full account data using
//!    `fill_buf_required(account_all_bytes_len)`
//!  * `consume(account_all_bytes_len)` to move to the next account
//!
//! When reading full accounts data whose sizes exceed the small stack buffer, the `BufReaderWithOverflow`
//! should be used, which supports dynamically allocated buffer for preparing contiguous data slices.
use {
    crate::{
        file_io::{read_into_buffer, read_more_buffer},
        io_setup::IoSetupState,
    },
    std::{
        fs::File,
        io::{self, BufRead},
        mem::MaybeUninit,
        ops::Range,
        path::Path,
        slice,
    },
};

/// A stack-allocated buffer.
///
/// This is a fixed-size buffer that is allocated on the stack.
///
/// This should be used when the required size is known at compile time and is within reasonable stack
/// limits.
struct Stack<const N: usize>([MaybeUninit<u8>; N]);

impl<const N: usize> Stack<N> {
    #[inline(always)]
    const fn new() -> Self {
        Self([MaybeUninit::uninit(); N])
    }
}

impl<const N: usize> Stack<N> {
    fn capacity(&self) -> usize {
        N
    }

    #[inline(always)]
    unsafe fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.0.as_ptr() as *const u8, N) }
    }

    #[inline(always)]
    unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, N) }
    }
}

/// An extension of the `BufRead` trait for file readers that allow tracking file
/// read position offset.
pub trait FileBufRead<'a>: BufRead {
    /// Activate the given `file` as source of reads of this reader.
    ///
    /// Resets the internal buffer to an empty state and sets the file offset to 0.
    ///
    /// `read_limit` provides a pre-defined limit on the number of bytes that can be read
    /// from the file (unless EOF is reached).
    fn set_file(&mut self, file: &'a File, read_limit: usize) -> io::Result<()>;

    /// Returns the current file offset corresponding to the start of the buffer
    /// that will be returned by the next call to `fill_buf`.
    ///
    /// This offset represents the position within the underlying file where data
    /// will be consumed from.
    fn get_file_offset(&self) -> usize;
}

/// An extension of the `BufRead` trait for readers that require stronger control
/// over returned buffer size.
///
/// Unlike the standard `BufRead`, which only guarantees a non-empty buffer,
/// this trait allows callers to enforce a minimum number of contiguous bytes
/// to be made available.
pub trait RequiredLenBufRead: BufRead {
    /// Ensures the internal buffer contains at least `required_len` contiguous bytes,
    /// and returns a slice of that buffer.
    ///
    /// Note: subsequent calls with the same or larger `required_len` are allowed, but
    /// before requesting smaller length all already provided bytes should be consumed
    /// using a single `consume` call.
    ///
    /// Returns `Err(io::ErrorKind::UnexpectedEof)` if the end of file is reached
    /// before the required number of bytes is available.
    ///
    /// Returns `Err(io::ErrorKind::QuotaExceeded)` if `required_len` exceeds supported limit.
    fn fill_buf_required(&mut self, required_len: usize) -> io::Result<&[u8]>;
}

pub trait RequiredLenBufFileRead<'a>: RequiredLenBufRead + FileBufRead<'a> {}
impl<'a, T: RequiredLenBufRead + FileBufRead<'a>> RequiredLenBufFileRead<'a> for T {}

/// read a file a large buffer at a time and provide access to a slice in that buffer
pub struct BufferedReader<'a, const N: usize> {
    /// when we are next asked to read from file, start at this offset
    file_offset_of_next_read: usize,
    /// the most recently read data. `buf_valid_bytes` specifies the range of `buf` that is valid.
    buf: Stack<N>,
    /// specifies the range of `buf` that contains valid data that has not been used by the caller
    buf_valid_bytes: Range<usize>,
    /// offset in the file of the `buf_valid_bytes`.`start`
    file_last_offset: usize,
    /// how many bytes are valid in the file. The file's len may be longer.
    file_len_valid: usize,
    /// reference to file handle
    file: Option<&'a File>,
}

impl<'a, const N: usize> BufferedReader<'a, N> {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            file_offset_of_next_read: 0,
            buf: Stack::new(),
            buf_valid_bytes: 0..0,
            file_last_offset: 0,
            file_len_valid: 0,
            file: None,
        }
    }

    pub fn with_file(mut self, file: &'a File, read_limit: usize) -> Self {
        self.do_set_file(file, read_limit);
        self
    }

    fn do_set_file(&mut self, file: &'a File, read_limit: usize) {
        self.file = Some(file);
        self.file_len_valid = read_limit;
        self.file_last_offset = 0;
        self.file_offset_of_next_read = 0;
        self.buf_valid_bytes = 0..0;
    }
}

impl<'a, const N: usize> FileBufRead<'a> for BufferedReader<'a, N> {
    fn set_file(&mut self, file: &'a File, read_limit: usize) -> io::Result<()> {
        self.do_set_file(file, read_limit);
        Ok(())
    }

    #[inline(always)]
    fn get_file_offset(&self) -> usize {
        if self.buf_valid_bytes.is_empty() {
            self.file_offset_of_next_read
        } else {
            self.file_last_offset + self.buf_valid_bytes.start
        }
    }
}

impl<const N: usize> BufferedReader<'_, N> {
    /// Defragment buffer and read more bytes to make sure we have filled available
    /// space as much as possible.
    fn read_more_bytes(&mut self) -> io::Result<()> {
        // we haven't used all the bytes we read last time, so adjust the effective offset
        debug_assert!(self.buf_valid_bytes.len() <= self.file_offset_of_next_read);
        self.file_last_offset = self.file_offset_of_next_read - self.buf_valid_bytes.len();
        let Some(file) = &self.file else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "no open file"));
        };
        read_more_buffer(
            file,
            self.file_len_valid,
            &mut self.file_offset_of_next_read,
            // SAFETY: `read_more_buffer` will only _write_ to uninitialized memory and lifetime is tied to self.
            unsafe { self.buf.as_mut_slice() },
            &mut self.buf_valid_bytes,
        )
    }

    fn valid_slice(&self) -> &[u8] {
        // SAFETY: We only read from memory that has been initialized by `read_more_buffer`
        // and lifetime is tied to self.
        unsafe { &self.buf.as_slice()[self.buf_valid_bytes.clone()] }
    }
}

impl<const N: usize> io::Read for BufferedReader<'_, N> {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let available_len = self.buf_valid_bytes.len();
        if available_len > 0 {
            // Copy already read data to buf.
            let available_valid_data = self.valid_slice();
            if available_len >= buf.len() {
                buf.copy_from_slice(&available_valid_data[..buf.len()]);
                self.consume(buf.len());
                return Ok(buf.len());
            }
            // Only part of the buffer can be filled.
            buf[..available_len].copy_from_slice(available_valid_data);
            buf = &mut buf[available_len..];
        }

        // Read directly from file into space still left in the buf.
        let Some(file) = &self.file else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "no open file"));
        };
        let bytes_read = read_into_buffer(
            file,
            self.file_len_valid,
            self.file_offset_of_next_read,
            buf,
        )?;
        let filled_len = bytes_read + available_len;
        // Buffer was successfully filled, drop buffered data and move offset.
        self.consume(filled_len);
        Ok(filled_len)
    }
}

/// `BufferedReader` implements a more permissive API compared to `BufRead`
/// by allowing `consume` to advance beyond the end of the buffer returned by `fill_buf`.
impl<const N: usize> BufRead for BufferedReader<'_, N> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.buf_valid_bytes.is_empty() {
            self.read_more_bytes()?;
        }
        Ok(self.valid_slice())
    }

    /// Advance the offset by `amt` to a `file` position where next `fill_buf` buffer should
    /// start at.
    ///
    /// Note that `amt` is not constrained by the size of the buffer returned by `fill_buf`
    /// and can be thus used to seek/skip reads from the underlying file.
    fn consume(&mut self, amt: usize) {
        if self.buf_valid_bytes.len() >= amt {
            self.buf_valid_bytes.start += amt;
        } else {
            let additional_amount_to_skip = amt - self.buf_valid_bytes.len();
            self.buf_valid_bytes = 0..0;
            self.file_offset_of_next_read += additional_amount_to_skip;
        }
    }
}

/// Supported `required_len` is limited by backing buffer size without ability to grow.
impl<const N: usize> RequiredLenBufRead for BufferedReader<'_, N> {
    fn fill_buf_required(&mut self, required_len: usize) -> io::Result<&[u8]> {
        if self.buf_valid_bytes.len() < required_len {
            self.read_more_bytes()?;
            if self.buf_valid_bytes.len() < required_len {
                if required_len > self.buf.capacity() {
                    return Err(io::Error::new(
                        io::ErrorKind::QuotaExceeded,
                        "requested more bytes than supported by buffer",
                    ));
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unable to read enough data",
                ));
            }
        }
        Ok(self.valid_slice())
    }
}

/// A buffered reader that wraps `BufRead` instance and implements `RequiredLenBufRead`.
///
/// It uses auxiliary overflow buffer when `fill_buf` returns slice that doesn't satisfy
/// the length requirement.
pub struct BufReaderWithOverflow<R> {
    reader: R,
    overflow_buf: Vec<u8>,
    overflow_min_capacity: usize,
    overflow_max_capacity: usize,
}

impl<R: BufRead> BufReaderWithOverflow<R> {
    pub fn new(reader: R, overflow_min_capacity: usize, overflow_max_capacity: usize) -> Self {
        Self {
            reader,
            overflow_buf: Vec::new(),
            overflow_min_capacity,
            overflow_max_capacity,
        }
    }
}

impl<R: BufRead> io::Read for BufReaderWithOverflow<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available_len = self.overflow_buf.len();
        if available_len == 0 {
            self.reader.read(buf)
        } else {
            assert!(
                buf.len() >= available_len,
                "should read all previously required bytes"
            );
            buf[..available_len].copy_from_slice(&self.overflow_buf);
            self.overflow_buf.clear();
            if buf.len() > available_len {
                let bytes_read = self.reader.read(&mut buf[available_len..])?;
                Ok(available_len + bytes_read)
            } else {
                Ok(available_len)
            }
        }
    }
}

impl<R: BufRead> BufRead for BufReaderWithOverflow<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.overflow_buf.is_empty() {
            self.reader.fill_buf()
        } else {
            Ok(self.overflow_buf.as_slice())
        }
    }

    fn consume(&mut self, mut amt: usize) {
        let overflow_len = self.overflow_buf.len();
        if overflow_len > 0 {
            amt = amt
                .checked_sub(overflow_len)
                .expect("should consume all previously required bytes");
            self.overflow_buf.clear();
        }
        self.reader.consume(amt);
    }
}

impl<'a, R: FileBufRead<'a>> FileBufRead<'a> for BufReaderWithOverflow<R> {
    fn set_file(&mut self, file: &'a File, read_limit: usize) -> io::Result<()> {
        self.overflow_buf.clear();
        self.reader.set_file(file, read_limit)
    }

    fn get_file_offset(&self) -> usize {
        self.reader.get_file_offset() - self.overflow_buf.len()
    }
}

/// Support large `required_len` (within configured limits) by using overflow buffer
/// retained during lifetime of the reader.
impl<R: BufRead> RequiredLenBufRead for BufReaderWithOverflow<R> {
    fn fill_buf_required(&mut self, required_len: usize) -> io::Result<&[u8]> {
        let available_len = self.overflow_buf.len();
        if available_len == 0 {
            let buf = self.reader.fill_buf()?;
            if buf.len() >= required_len {
                // Separate fill_buf call is needed due to borrow checker's limitation
                // https://rust-lang.github.io/rfcs/2094-nll.html#problem-case-3-conditional-control-flow-across-functions
                return self.reader.fill_buf();
            }
        }
        assert!(
            available_len <= required_len,
            "fill_buf_required should keep or grow required_len until consume"
        );
        if required_len > self.overflow_buf.capacity() {
            let target_capacity = required_len
                .next_power_of_two()
                .clamp(self.overflow_min_capacity, self.overflow_max_capacity);
            if required_len > target_capacity {
                return Err(io::Error::new(
                    io::ErrorKind::QuotaExceeded,
                    "requested more bytes than allowed capacity range",
                ));
            }
            self.overflow_buf
                .reserve_exact(target_capacity - available_len);
        }
        // Safety: we have reserved capacity and all of it will be filled by read
        unsafe { self.overflow_buf.set_len(required_len) };

        // On error overflow buffer is completely cleared to avoid access to
        // uninitialized memory.
        self.reader
            .read_exact(&mut self.overflow_buf[available_len..])
            .inspect_err(|_| self.overflow_buf.clear())?;
        Ok(self.overflow_buf.as_slice())
    }
}

/// Open file at `path` with buffering reader using `buf_size` memory and doing
/// read-ahead IO reads (if `io_uring` is supported by the platform)
pub fn large_file_buf_reader(
    path: &Path,
    buf_size: usize,
    io_setup: &IoSetupState,
) -> io::Result<impl BufRead + use<>> {
    #[cfg(target_os = "linux")]
    {
        assert!(agave_io_uring::io_uring_supported());
        use crate::io_uring::sequential_file_reader::SequentialFileReaderBuilder;

        SequentialFileReaderBuilder::new()
            .shared_sqpoll(io_setup.shared_sqpoll_fd())
            .build(path, buf_size)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::io::BufReader;
        let file = File::open(path)?;
        let _ = io_setup;
        Ok(BufReader::with_capacity(buf_size, file))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::io::{Read as _, Write},
        tempfile::tempfile,
    };

    #[inline(always)]
    fn rand_bytes<const N: usize>() -> [u8; N] {
        use rand::Rng;
        let mut rng = rand::rng();
        std::array::from_fn(|_| rng.random::<u8>())
    }

    #[test]
    fn test_buffered_reader() {
        // Setup a sample file with 32 bytes of data read using 16 bytes buffer
        const BUFFER_SIZE: usize = 16;
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // First read 16 bytes to fill buffer
        let file_len_valid = 32;
        let default_min_read = 8;
        let mut reader =
            BufferedReader::<BUFFER_SIZE>::new().with_file(&sample_file, file_len_valid);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(default_min_read).unwrap();
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), BUFFER_SIZE);
        assert_eq!(slice, &bytes[0..BUFFER_SIZE]);

        // Consume the data and attempt to read next 32 bytes, which is above supported buffer size,
        // so file offset is moved, but call returns quota error.
        let advance = 16;
        let mut required_len = 32;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader
                .fill_buf_required(required_len)
                .expect_err("should fail due to required length above buffer size")
                .kind(),
            io::ErrorKind::QuotaExceeded
        );

        // Continue reading should yield EOF.
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        required_len = 16;
        assert_eq!(
            reader
                .fill_buf_required(required_len)
                .expect_err("should hit EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        // set_required_data to zero and offset should not change, and slice should be empty.
        required_len = 0;
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(required_len).unwrap();
        let expected_offset = file_len_valid;
        assert_eq!(offset, expected_offset);
        let expected_slice_len = 0;
        assert_eq!(slice.len(), expected_slice_len);
    }

    #[test]
    fn test_buffered_reader_with_extra_data_in_file() {
        // Setup a sample file with 32 bytes of data read using 16 bytes buffer
        const BUFFER_SIZE: usize = 16;
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // Set file valid_len to 30 (i.e. 2 garbage bytes at the end of the file)
        let valid_len = 30;

        // First read 16 bytes to fill buffer
        let default_min_read_size = 8;
        let mut reader = BufferedReader::<BUFFER_SIZE>::new().with_file(&sample_file, valid_len);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(default_min_read_size).unwrap();
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), BUFFER_SIZE);
        assert_eq!(slice, &bytes[0..BUFFER_SIZE]);

        // Consume the data and attempt read next 16 bytes, expect to hit `valid_len`, and only read 14 bytes
        let mut advance = 16;
        let mut required_data_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader
                .fill_buf_required(required_data_len)
                .expect_err("should hit EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Continue reading should yield EOF.
        advance = 14;
        required_data_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader
                .fill_buf_required(required_data_len)
                .expect_err("should hit EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Move the offset passed `valid_len`, expect to hit EOF.
        advance = 1;
        required_data_len = 8;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader
                .fill_buf_required(required_data_len)
                .expect_err("should hit EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Move the offset passed file_len, expect to hit EOF.
        advance = 3;
        required_data_len = 8;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader
                .fill_buf_required(required_data_len)
                .expect_err("Should hit EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_buffered_reader_partial_consume() {
        // Setup a sample file with 32 bytes of data read using 16 bytes buffer
        const BUFFER_SIZE: usize = 16;
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // First read 16 bytes to fill buffer
        let file_len_valid = 32;
        let default_min_read_size = 8;
        let mut reader =
            BufferedReader::<BUFFER_SIZE>::new().with_file(&sample_file, file_len_valid);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(default_min_read_size).unwrap();
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), BUFFER_SIZE);
        assert_eq!(slice, &bytes[0..BUFFER_SIZE]);

        // Consume the partial data (8 byte) and attempt to read next 8 bytes
        let mut advance = 8;
        let mut required_len = 8;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(required_len).unwrap();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_len);
        assert_eq!(
            slice,
            &bytes[expected_offset..expected_offset + required_len]
        ); // no need to read more

        // Continue reading should succeed and read the rest 16 bytes.
        advance = 8;
        required_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(required_len).unwrap();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_len);
        assert_eq!(
            slice,
            &bytes[expected_offset..expected_offset + required_len]
        );

        // Continue reading should yield EOF and empty slice.
        advance = 16;
        required_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader
                .fill_buf_required(required_len)
                .expect_err("should hit EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Attempt to read more than the buffer size
        required_len = 32;
        assert_eq!(
            reader
                .fill_buf_required(required_len)
                .expect_err("should fail due to too large required length")
                .kind(),
            io::ErrorKind::QuotaExceeded
        );
    }

    #[test]
    fn test_buffered_reader_partial_consume_with_move() {
        // Setup a sample file with 32 bytes of data read using 16 bytes buffer
        const BUFFER_SIZE: usize = 16;
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // First read 16 bytes to fill buffer
        let valid_len = 32;
        let default_min_read = 8;
        let mut reader = BufferedReader::<BUFFER_SIZE>::new().with_file(&sample_file, valid_len);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(default_min_read).unwrap();
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), BUFFER_SIZE);
        assert_eq!(slice, &bytes[0..BUFFER_SIZE]);

        // Consume the partial data (8 bytes) and attempt to read next 16 bytes
        // This will move the leftover 8bytes and read next 8 bytes.
        let mut advance = 8;
        let mut required_data_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(required_data_len).unwrap();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_data_len);
        assert_eq!(
            slice,
            &bytes[expected_offset..expected_offset + required_data_len]
        );

        // Continue reading should succeed and read the rest 8 bytes.
        advance = 16;
        required_data_len = 8;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = reader.fill_buf_required(required_data_len).unwrap();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_data_len);
        assert_eq!(
            slice,
            &bytes[expected_offset..expected_offset + required_data_len]
        );
    }

    #[test]
    fn test_fill_buf_required_or_overflow() {
        // Setup a sample file with 32 bytes of data read using 16 bytes buffer
        const BUFFER_SIZE: usize = 16;
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        let mut reader = BufReaderWithOverflow::new(
            BufferedReader::<BUFFER_SIZE>::new().with_file(&sample_file, FILE_SIZE),
            0,
            usize::MAX,
        );

        // Case 1: required_len <= BUFFER_SIZE (no overflow needed)
        let required_len = 8;
        let slice = reader.fill_buf_required(required_len).unwrap();
        assert_eq!(&slice[..required_len], &bytes[..required_len]);

        // Consume part of the buffer to simulate partial reading
        reader.consume(required_len);

        // Case 2: required_len > buffer_size (overflow required)
        let required_len = BUFFER_SIZE + 8;
        let slice = reader.fill_buf_required(required_len).unwrap();

        // Internal buffer is size `buffer_size`, overflow should extend with the remaining `8` bytes
        assert_eq!(slice.len(), required_len);
        assert_eq!(slice, &bytes[8..8 + required_len]);

        // Consume everything to reach EOF
        reader.consume(required_len);

        // Case 3: required_len larger than remaining data (expect UnexpectedEof)
        let required_len = 64;
        let result = reader.fill_buf_required(required_len);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);

        // Case 4: required_len = 0 (should return empty slice)
        let required_len = 0;
        let offset_before = reader.get_file_offset();
        let slice = reader.fill_buf_required(required_len).unwrap();
        assert_eq!(slice.len(), 0);
        let offset_after = reader.get_file_offset();
        assert_eq!(offset_before, offset_after);
    }

    #[test]
    fn test_overflow_reader_read_and_fill_buf() {
        const BUFFER_SIZE: usize = 16;
        const FILE_SIZE: usize = 64;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        let mut reader = BufReaderWithOverflow::new(
            BufferedReader::<BUFFER_SIZE>::new().with_file(&sample_file, FILE_SIZE),
            0,
            32,
        );
        let buf = reader.fill_buf().unwrap();
        assert_eq!(buf, &bytes[0..BUFFER_SIZE]);

        reader.consume(8);
        let mut buf = [0; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 8);
        assert_eq!(buf, &bytes[8..BUFFER_SIZE]);

        assert_eq!(
            reader
                .fill_buf_required(40)
                .expect_err("should exceed len limit")
                .kind(),
            io::ErrorKind::QuotaExceeded
        );

        // Required buffer is at maximum configured limit.
        let buf = reader.fill_buf_required(32).unwrap();
        assert_eq!(buf, &bytes[BUFFER_SIZE..BUFFER_SIZE + 32]);
        // Same buffer should be returned.
        let buf = reader.fill_buf().unwrap();
        assert_eq!(buf, &bytes[BUFFER_SIZE..BUFFER_SIZE + 32]);

        let mut buf = [0; 48];
        assert_eq!(reader.read(&mut buf).unwrap(), 48);
        assert_eq!(buf, &bytes[BUFFER_SIZE..BUFFER_SIZE + 48]);

        assert_eq!(reader.read(&mut buf).unwrap(), 0);

        assert_eq!(
            reader
                .fill_buf_required(1)
                .expect_err("should reach EOF")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
