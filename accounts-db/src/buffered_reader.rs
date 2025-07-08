//! File I/O buffered reader for AppendVec
//! Specialized `BufRead`-like type for reading account data.
//!
//! Callers can use this type to iterate efficiently over append vecs. They can do so by repeatedly
//! calling `fill_buf()`, `consume()` and `set_required_data_len(account_data_len)` once the next account
//! data length is known.
//!
//! Unlike BufRead/BufReader, this type guarantees that on the next `fill_buf()` after calling
//! `set_required_data_len(len)`, the whole account data is buffered _linearly_ in memory and available to
//! be returned.
use {
    crate::file_io::{read_into_buffer, read_more_buffer},
    std::{
        fs::File,
        io::{self, BufRead, BufReader},
        mem::MaybeUninit,
        ops::Range,
        path::Path,
        slice,
    },
};

/// A trait that abstracts over the backing storage of the buffer.
///
/// This allows flexibility in the type of buffer used. For example, depending on the required size, a
/// caller may be able to opt for a stack-allocated buffer rather than a heap-allocated buffer, or
/// vice versa.
pub(crate) trait Backing {
    fn capacity(&self) -> usize;
    unsafe fn as_slice(&self) -> &[u8];
    unsafe fn as_mut_slice(&mut self) -> &mut [u8];
}

/// A stack-allocated buffer.
///
/// This is a fixed-size buffer that is allocated on the stack.
///
/// This should be used when the required size is known at compile time and is within reasonable stack
/// limits.
pub(crate) struct Stack<const N: usize>([MaybeUninit<u8>; N]);

impl<const N: usize> Stack<N> {
    #[inline(always)]
    pub fn new() -> Self {
        Self([MaybeUninit::uninit(); N])
    }
}

impl<const N: usize> Backing for Stack<N> {
    fn capacity(&self) -> usize {
        N
    }

    #[inline(always)]
    unsafe fn as_slice(&self) -> &[u8] {
        slice::from_raw_parts(self.0.as_ptr() as *const u8, N)
    }

    #[inline(always)]
    unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, N)
    }
}

/// An extension of the `BufRead` trait for file readers that require stronger control
/// over returned buffer size and tracking of the file offset.
///
/// Unlike the standard `fill_buf`, which only guarantees a non-empty buffer,
/// this trait allows callers to:
/// - Enforce a minimum number of contiguous bytes to be made available.
/// - Fall back to an overflow buffer if the internal buffer cannot satisfy the request.
/// - Retrieve the current file offset corresponding to the start of the next buffer.
pub(crate) trait ContiguousBufFileRead<'a>: BufRead {
    /// Returns the current file offset corresponding to the start of the buffer
    /// that will be returned by the next call to `fill_buf_*`.
    ///
    /// This offset represents the position within the underlying file where data
    /// will be consumed from.
    fn get_file_offset(&self) -> usize;

    /// Ensures the internal buffer contains at least `required_len` contiguous bytes,
    /// and returns a slice to that buffer.
    ///
    /// Returns `Err(io::ErrorKind::UnexpectedEof)` if the end of file is reached
    /// before the required number of bytes is available.
    fn fill_buf_required(&mut self, required_len: usize) -> io::Result<&[u8]>;

    /// Attempts to provide at least `required_len` contiguous bytes by using
    /// the internal buffer or the provided `overflow_buffer` if needed.
    ///
    /// If the internal buffer alone does not satisfy the requirement, additional
    /// bytes are read and appended to `overflow_buffer`, which is resized to fit the data.
    ///
    /// Returns a slice containing all the required data (may point to either buffer).
    ///
    /// Returns `Err(io::ErrorKind::UnexpectedEof)` if the end of file is reached
    /// before the required number of bytes can be read.
    fn fill_buf_required_or_overflow<'b>(
        &'b mut self,
        required_len: usize,
        overflow_buffer: &'b mut Vec<u8>,
    ) -> io::Result<&'b [u8]>
    where
        'a: 'b;
}

/// read a file a large buffer at a time and provide access to a slice in that buffer
pub struct BufferedReader<'a, T> {
    /// when we are next asked to read from file, start at this offset
    file_offset_of_next_read: usize,
    /// the most recently read data. `buf_valid_bytes` specifies the range of `buf` that is valid.
    buf: T,
    /// specifies the range of `buf` that contains valid data that has not been used by the caller
    buf_valid_bytes: Range<usize>,
    /// offset in the file of the `buf_valid_bytes`.`start`
    file_last_offset: usize,
    /// how many bytes are valid in the file. The file's len may be longer.
    file_len_valid: usize,
    /// reference to file handle
    file: &'a File,
}

impl<'a, T> BufferedReader<'a, T> {
    /// `buffer_size`: how much to try to read at a time
    /// `file_len_valid`: # bytes that are valid in the file, may be less than overall file len
    /// `default_min_read_requirement`: make sure we always have this much data available if we're asked to read
    pub fn new(backing: T, file_len_valid: usize, file: &'a File) -> Self {
        Self {
            file_offset_of_next_read: 0,
            buf: backing,
            buf_valid_bytes: 0..0,
            file_last_offset: 0,
            file_len_valid,
            file,
        }
    }
}

impl<'a, T: Backing> ContiguousBufFileRead<'a> for BufferedReader<'a, T> {
    #[inline(always)]
    fn get_file_offset(&self) -> usize {
        if self.buf_valid_bytes.is_empty() {
            self.file_offset_of_next_read
        } else {
            self.file_last_offset + self.buf_valid_bytes.start
        }
    }

    fn fill_buf_required(&mut self, required_len: usize) -> io::Result<&[u8]> {
        if self.buf_valid_bytes.len() < required_len {
            self.read_more_bytes()?;
            if self.buf_valid_bytes.len() < required_len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unable to read enough data",
                ));
            }
        }
        Ok(self.valid_slice())
    }

    fn fill_buf_required_or_overflow<'b>(
        &'b mut self,
        required_len: usize,
        overflow_buffer: &'b mut Vec<u8>,
    ) -> io::Result<&'b [u8]>
    where
        'a: 'b,
    {
        if required_len <= self.buf.capacity() {
            return self.fill_buf_required(required_len);
        }

        if required_len > overflow_buffer.capacity() {
            overflow_buffer.reserve_exact(required_len - overflow_buffer.len());
        }
        // SAFETY: We only write to the uninitialized portion of the buffer via `copy_from_slice` and `read_into_buffer`.
        // Later, we ensure we only read from the initialized portion of the buffer.
        unsafe {
            overflow_buffer.set_len(required_len);
        }

        // Copy already read data to overflow buffer.
        let available_valid_data = self.valid_slice();
        let leftover = available_valid_data.len();
        overflow_buffer[..leftover].copy_from_slice(available_valid_data);

        // Read remaining data into overflow buffer.
        let read_dst = &mut overflow_buffer[leftover..];
        let bytes_read = read_into_buffer(
            self.file,
            self.file_len_valid,
            self.file_offset_of_next_read,
            read_dst,
        )?;
        if bytes_read < read_dst.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unable to read required amount of data",
            ));
        }
        Ok(overflow_buffer.as_slice())
    }
}

impl<T> BufferedReader<'_, T>
where
    T: Backing,
{
    /// Defragment buffer and read more bytes to make sure we have filled available
    /// space as much as possible.
    fn read_more_bytes(&mut self) -> io::Result<()> {
        // we haven't used all the bytes we read last time, so adjust the effective offset
        debug_assert!(self.buf_valid_bytes.len() <= self.file_offset_of_next_read);
        self.file_last_offset = self.file_offset_of_next_read - self.buf_valid_bytes.len();
        read_more_buffer(
            self.file,
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

impl<'a, const N: usize> BufferedReader<'a, Stack<N>> {
    /// create a new buffered reader with a stack-allocated buffer
    pub fn new_stack(file_len_valid: usize, file: &'a File) -> Self {
        BufferedReader::new(Stack::new(), file_len_valid, file)
    }
}

impl<T: Backing> io::Read for BufferedReader<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        if available.is_empty() {
            return Ok(0);
        }
        let bytes_to_read = available.len().min(buf.len());
        buf[..bytes_to_read].copy_from_slice(&available[..bytes_to_read]);
        self.consume(bytes_to_read);
        Ok(bytes_to_read)
    }
}

/// `BufferedReader` implements a more permissive API compared to `BufRead`
/// by allowing `consume` to advance beyond the end of the buffer returned by `fill_buf`.
impl<T: Backing> BufRead for BufferedReader<'_, T> {
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

/// Open file at `path` with buffering reader using `buf_size` memory and doing
/// read-ahead IO reads (if `io_uring` is supported by the host)
pub fn large_file_buf_reader(
    path: impl AsRef<Path>,
    buf_size: usize,
) -> io::Result<Box<dyn BufRead>> {
    #[cfg(target_os = "linux")]
    if agave_io_uring::io_uring_supported() {
        use crate::io_uring::sequential_file_reader::SequentialFileReader;

        let io_uring_reader = SequentialFileReader::with_capacity(buf_size, &path);
        match io_uring_reader {
            Ok(reader) => return Ok(Box::new(reader)),
            Err(error) => {
                log::warn!("unable to create io_uring reader: {error}");
            }
        }
    }
    let file = File::open(path)?;
    Ok(Box::new(BufReader::with_capacity(buf_size, file)))
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::append_vec::ValidSlice, std::io::Write, tempfile::tempfile,
        test_case::test_case,
    };

    #[inline(always)]
    fn rand_bytes<const N: usize>() -> [u8; N] {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        std::array::from_fn(|_| rng.gen::<u8>())
    }

    #[test_case(Stack::<16>::new(), 16)]
    fn test_buffered_reader(backing: impl Backing, buffer_size: usize) {
        // Setup a sample file with 32 bytes of data
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // First read 16 bytes to fill buffer
        let file_len_valid = 32;
        let default_min_read = 8;
        let mut reader = BufferedReader::new(backing, file_len_valid, &sample_file);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(default_min_read).unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the data and attempt to read next 32 bytes, expect to hit EOF
        let advance = 16;
        let mut required_len = 32;
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

        // Continue reading should yield EOF.
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

        // set_required_data to zero and offset should not change, and slice should be empty.
        required_len = 0;
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(required_len).unwrap());
        let expected_offset = file_len_valid;
        assert_eq!(offset, expected_offset);
        let expected_slice_len = 0;
        assert_eq!(slice.len(), expected_slice_len);
    }

    #[test_case(Stack::<16>::new(), 16)]
    fn test_buffered_reader_with_extra_data_in_file(backing: impl Backing, buffer_size: usize) {
        // Setup a sample file with 32 bytes of data
        let mut sample_file = tempfile().unwrap();
        const FILE_SIZE: usize = 32;
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // Set file valid_len to 30 (i.e. 2 garbage bytes at the end of the file)
        let valid_len = 30;

        // First read 16 bytes to fill buffer
        let default_min_read_size = 8;
        let mut reader = BufferedReader::new(backing, valid_len, &sample_file);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(default_min_read_size).unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the data and attempt read next 32 bytes, expect to hit `valid_len`, and only read 14 bytes
        let mut advance = 16;
        let mut required_data_len = 32;
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
        required_data_len = 32;
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

    #[test_case(Stack::<16>::new(), 16)]
    fn test_buffered_reader_partial_consume(backing: impl Backing, buffer_size: usize) {
        // Setup a sample file with 32 bytes of data
        let mut sample_file = tempfile().unwrap();
        const FILE_SIZE: usize = 32;
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // First read 16 bytes to fill buffer
        let file_len_valid = 32;
        let default_min_read_size = 8;
        let mut reader = BufferedReader::new(backing, file_len_valid, &sample_file);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(default_min_read_size).unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the partial data (8 byte) and attempt to read next 8 bytes
        let mut advance = 8;
        let mut required_len = 8;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(required_len).unwrap());
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_len);
        assert_eq!(
            slice.slice(),
            &bytes[expected_offset..expected_offset + required_len]
        ); // no need to read more

        // Continue reading should succeed and read the rest 16 bytes.
        advance = 8;
        required_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(required_len).unwrap());
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_len);
        assert_eq!(
            slice.slice(),
            &bytes[expected_offset..expected_offset + required_len]
        );

        // Continue reading should yield EOF and empty slice.
        advance = 16;
        required_len = 32;
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
    }

    #[test_case(Stack::<16>::new(), 16)]
    fn test_buffered_reader_partial_consume_with_move(backing: impl Backing, buffer_size: usize) {
        // Setup a sample file with 32 bytes of data
        let mut sample_file = tempfile().unwrap();
        const FILE_SIZE: usize = 32;
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        // First read 16 bytes to fill buffer
        let valid_len = 32;
        let default_min_read = 8;
        let mut reader = BufferedReader::new(backing, valid_len, &sample_file);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(default_min_read).unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the partial data (8 bytes) and attempt to read next 16 bytes
        // This will move the leftover 8bytes and read next 8 bytes.
        let mut advance = 8;
        let mut required_data_len = 16;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(required_data_len).unwrap());
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_data_len);
        assert_eq!(
            slice.slice(),
            &bytes[expected_offset..expected_offset + required_data_len]
        );

        // Continue reading should succeed and read the rest 8 bytes.
        advance = 16;
        required_data_len = 8;
        reader.consume(advance);
        let offset = reader.get_file_offset();
        let slice = ValidSlice::new(reader.fill_buf_required(required_data_len).unwrap());
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_data_len);
        assert_eq!(
            slice.slice(),
            &bytes[expected_offset..expected_offset + required_data_len]
        );
    }

    #[test_case(Stack::<16>::new(), 16)]
    fn test_fill_buf_required_or_overflow(backing: impl Backing, buffer_size: usize) {
        // Setup a sample file with 32 bytes of data
        const FILE_SIZE: usize = 32;
        let mut sample_file = tempfile().unwrap();
        let bytes = rand_bytes::<FILE_SIZE>();
        sample_file.write_all(&bytes).unwrap();

        let file_len_valid = 32;
        let mut reader = BufferedReader::new(backing, file_len_valid, &sample_file);

        // Case 1: required_len <= buffer_size (no overflow needed)
        let mut overflow = Vec::new();
        let required_len = 8;
        let slice = reader
            .fill_buf_required_or_overflow(required_len, &mut overflow)
            .unwrap();
        assert_eq!(&slice[..required_len], &bytes[..required_len]);
        assert!(overflow.is_empty());

        // Consume part of the buffer to simulate partial reading
        reader.consume(required_len);

        // Case 2: required_len > buffer_size (overflow required)
        let mut overflow = Vec::new();
        let required_len = buffer_size + 8;
        let slice = reader
            .fill_buf_required_or_overflow(required_len, &mut overflow)
            .unwrap();

        // Internal buffer is size `buffer_size`, overflow should extend with the remaining `8` bytes
        assert_eq!(slice.len(), required_len);
        assert_eq!(slice, &bytes[8..8 + required_len]);
        assert_eq!(overflow.len(), required_len);

        // Consume everything to reach EOF
        reader.consume(required_len);

        // Case 3: required_len larger than remaining data (expect UnexpectedEof)
        let mut overflow = Vec::new();
        let required_len = 64;
        let result = reader.fill_buf_required_or_overflow(required_len, &mut overflow);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);

        // Case 4: required_len = 0 (should return empty slice)
        let mut overflow = Vec::new();
        let required_len = 0;
        let offset_before = reader.get_file_offset();
        let slice = reader
            .fill_buf_required_or_overflow(required_len, &mut overflow)
            .unwrap();
        assert_eq!(slice.len(), 0);
        let offset_after = reader.get_file_offset();
        assert_eq!(offset_before, offset_after);
    }
}
