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
    crate::file_io::read_more_buffer,
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
    #[inline(always)]
    unsafe fn as_slice(&self) -> &[u8] {
        slice::from_raw_parts(self.0.as_ptr() as *const u8, N)
    }

    #[inline(always)]
    unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        slice::from_raw_parts_mut(self.0.as_mut_ptr() as *mut u8, N)
    }
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
    /// how many contiguous bytes caller needs
    read_requirements: Option<usize>,
    /// how many bytes are valid in the file. The file's len may be longer.
    file_len_valid: usize,
    /// reference to file handle
    file: &'a File,
    /// we always want at least this many contiguous bytes available or we must read more into the buffer.
    default_min_read_requirement: usize,
}

impl<'a, T> BufferedReader<'a, T> {
    /// `buffer_size`: how much to try to read at a time
    /// `file_len_valid`: # bytes that are valid in the file, may be less than overall file len
    /// `default_min_read_requirement`: make sure we always have this much data available if we're asked to read
    pub fn new(
        backing: T,
        file_len_valid: usize,
        file: &'a File,
        default_min_read_requirement: usize,
    ) -> Self {
        Self {
            file_offset_of_next_read: 0,
            buf: backing,
            buf_valid_bytes: 0..0,
            file_last_offset: 0,
            read_requirements: None,
            file_len_valid,
            file,
            default_min_read_requirement,
        }
    }

    /// specify the amount of data required to read next time `read` is called
    #[inline(always)]
    pub fn set_required_data_len(&mut self, len: usize) {
        self.read_requirements = Some(len);
    }
}

impl<'a, T> BufferedReader<'a, T>
where
    T: Backing,
{
    /// read to make sure we have the minimum amount of data
    fn read_required_bytes(&mut self) -> io::Result<()> {
        let must_read = self
            .read_requirements
            .unwrap_or(self.default_min_read_requirement);
        if self.buf_valid_bytes.len() < must_read {
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
            )?;
            if self.buf_valid_bytes.len() < must_read {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unable to read enough data",
                ));
            }
        }
        // reset this once we have checked that we had this much data once
        self.read_requirements = None;
        Ok(())
    }

    /// Return file offset within `file` of the current consume position.
    ///
    /// The offset is corresponding to the start of buffer that will be returned
    /// by the next `fill_buf` call.
    #[inline(always)]
    pub fn get_offset(&'a self) -> usize {
        if self.buf_valid_bytes.is_empty() {
            self.file_offset_of_next_read
        } else {
            self.file_last_offset + self.buf_valid_bytes.start
        }
    }
}

impl<'a, const N: usize> BufferedReader<'a, Stack<N>> {
    /// create a new buffered reader with a stack-allocated buffer
    pub fn new_stack(
        file_len_valid: usize,
        file: &'a File,
        default_min_read_requirement: usize,
    ) -> Self {
        BufferedReader::new(
            Stack::new(),
            file_len_valid,
            file,
            default_min_read_requirement,
        )
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
    /// Return the biggest slice of valid data starting at the current offset.
    ///
    /// Note that `fill_buf` has stronger guarantee than `BufRead::fill_buf` and returns
    /// at least the number of bytes requested by `default_min_read_requirement` and
    /// `set_required_data_len`. If that condition cannot be met
    /// `Err(io::ErrorKind::UnexpectedEof)` is returned.
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.read_required_bytes()?;

        // SAFETY: We only read from memory that has been initialized by `read_more_buffer`
        // and lifetime is tied to self.
        Ok(unsafe { &self.buf.as_slice()[self.buf_valid_bytes.clone()] })
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

        let io_uring_reader = SequentialFileReader::with_capacity(buf_size, path.as_ref());
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
        let mut reader =
            BufferedReader::new(backing, file_len_valid, &sample_file, default_min_read);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the data and attempt to read next 32 bytes, expect to hit EOF
        let advance = 16;
        let mut required_len = 32;
        reader.consume(advance);
        reader.set_required_data_len(required_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("should hit EOF").kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Continue reading should yield EOF.
        reader.consume(advance);
        reader.set_required_data_len(required_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("should hit EOF").kind(),
            io::ErrorKind::UnexpectedEof
        );

        // set_required_data to zero and offset should not change, and slice should be empty.
        required_len = 0;
        reader.set_required_data_len(required_len);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
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
        let mut reader =
            BufferedReader::new(backing, valid_len, &sample_file, default_min_read_size);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the data and attempt read next 32 bytes, expect to hit `valid_len`, and only read 14 bytes
        let mut advance = 16;
        let mut required_data_len = 32;
        reader.consume(advance);
        reader.set_required_data_len(required_data_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("should hit EOF").kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Continue reading should yield EOF.
        advance = 14;
        required_data_len = 32;
        reader.consume(advance);
        reader.set_required_data_len(required_data_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("should hit EOF").kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Move the offset passed `valid_len`, expect to hit EOF.
        advance = 1;
        required_data_len = 8;
        reader.consume(advance);
        reader.set_required_data_len(required_data_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("should hit EOF").kind(),
            io::ErrorKind::UnexpectedEof
        );

        // Move the offset passed file_len, expect to hit EOF.
        advance = 3;
        required_data_len = 8;
        reader.consume(advance);
        reader.set_required_data_len(required_data_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("Should hit EOF").kind(),
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
        let mut reader =
            BufferedReader::new(backing, file_len_valid, &sample_file, default_min_read_size);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the partial data (8 byte) and attempt to read next 8 bytes
        let mut advance = 8;
        let mut required_len = 8;
        reader.consume(advance);
        reader.set_required_data_len(required_len);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
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
        reader.set_required_data_len(required_len);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
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
        reader.set_required_data_len(required_len);
        let offset = reader.get_offset();
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(
            reader.fill_buf().expect_err("should hit EOF").kind(),
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
        let mut reader = BufferedReader::new(backing, valid_len, &sample_file, default_min_read);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
        let mut expected_offset = 0;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), buffer_size);
        assert_eq!(slice.slice(), &bytes[0..buffer_size]);

        // Consume the partial data (8 bytes) and attempt to read next 16 bytes
        // This will move the leftover 8bytes and read next 8 bytes.
        let mut advance = 8;
        let mut required_data_len = 16;
        reader.consume(advance);
        reader.set_required_data_len(required_data_len);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
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
        reader.set_required_data_len(required_data_len);
        let offset = reader.get_offset();
        let slice = ValidSlice::new(reader.fill_buf().unwrap());
        expected_offset += advance;
        assert_eq!(offset, expected_offset);
        assert_eq!(slice.len(), required_data_len);
        assert_eq!(
            slice.slice(),
            &bytes[expected_offset..expected_offset + required_data_len]
        );
    }
}
