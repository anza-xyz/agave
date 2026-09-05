//! data file layout:
//!
//!  +-------------+-------------------+-----------------------+
//!  |      Offset | Region            |                  Size |
//!  +-------------+-------------------+-----------------------+
//!  |           0 | data header       |                  64 B |
//!  +-------------+-------------------+-----------------------+
//!  |          64 | alignment padding |                4032 B |
//!  +-------------+-------------------+-----------------------+
//!  |        4096 | data entry 0      |       36 B + data_len |
//!  +-------------+-------------------+-----------------------+
//!  |             | alignment padding | to 4096-byte boundary |
//!  +-------------+-------------------+-----------------------+
//!  | align(4096) | data entry 1      |       36 B + data_len |
//!  +-------------+-------------------+-----------------------+
//!  |             | alignment padding | to 4096-byte boundary |
//!  +-------------+-------------------+-----------------------+
//!  | align(4096) | data entry 2      |       36 B + data_len |
//!  +-------------+-------------------+-----------------------+
//!  |             | ...               |                       |
//!  +-------------+-------------------+-----------------------+
//!
//! data header layout:
//!
//!  +--------+---------------+----------+-------+
//!  | Offset | Field         | Type     |  Size |
//!  +--------+---------------+----------+-------+
//!  |      0 | magic         | [u8; 16] |  16 B |
//!  +--------+----------------+----------+-------+
//!  |     16 | major_version | u64      |   8 B |
//!  +--------+----------------+----------+-------+
//!  |     24 | minor_version | u64      |   8 B |
//!  +--------+----------------+----------+-------+
//!  |     32 | patch_version | u64      |   8 B |
//!  +--------+---------------+----------+-------+
//!  |     40 | reserved      | [u8; 24] |  24 B |
//!  +--------+---------------+----------+-------+
//!
//!  - total size: 64 B
//!  - the version fields combine to follow semver
//!
//! data entry layout:
//!
//!  +--------+----------+----------------+----------+
//!  | Offset | Field    | Type           |     Size |
//!  +--------+----------+----------------+----------+
//!  |      0 | address  | Pubkey         |     32 B |
//!  +--------+----------+----------------+----------+
//!  |     32 | data_len | u32            |      4 B |
//!  +--------+----------+----------------+----------+
//!  |     36 | data     | [u8; data_len] | data_len |
//!  +--------+----------+----------------+----------+
//!
//!  - pubkey and data_len fields are used for validation with meta entry
//!  - data entries are written at file offsets aligned to 4,096

use {
    super::{
        SplitAccountsFileError,
        as_bytes::{AsBytesMut, AsBytesRef, as_bytes_mut, as_bytes_ref},
        common::{DataLen, FileOffset, WriteInfo},
        error::{
            ReadDataEntryError, ReadDataHeaderError, WriteDataEntryError, WriteDataHeaderError,
        },
        utils,
    },
    agave_fs::{FileSize, file_io},
    semver::Version,
    solana_pubkey::Pubkey,
    std::{
        convert::TryFrom,
        fs::File,
        path::{Path, PathBuf},
        ptr, slice,
    },
};

pub const DATA_HEADER_SIZE: usize = 64;

const DATA_MAGIC: &[u8; 16] = b"agave data file\0";
const DATA_FORMAT_VERSION: Version = Version::new(0, 1, 0);

/// data entries are always written at offsets that are multiples of this alignment
pub const DATA_ENTRY_OFFSET_ALIGNMENT: usize = 1 << DATA_ENTRY_OFFSET_ALIGNMENT_LOG2;
pub const DATA_ENTRY_OFFSET_ALIGNMENT_LOG2: u32 = 12;

/// Size, in bytes, of a data entry's fixed portion (i.e. *not* the actual account data).
pub const DATA_ENTRY_FIXED_SIZE: usize = 36;

// byte-offsets of the fields within a data entry
const DATA_ENTRY_OFFSET_OF_ADDRESS: usize = 0;
const DATA_ENTRY_OFFSET_OF_DATA_LEN: usize = 32;
const DATA_ENTRY_OFFSET_OF_DATA: usize = 36;

/// Creates a new data file based on `base_path`.
pub fn create_data_file(
    base_path: impl AsRef<Path>,
) -> Result<(PathBuf, File, usize), SplitAccountsFileError> {
    let data_path = data_path_from_base(&base_path);
    let mut data_file = utils::create_new_file(&data_path)?;
    let header_size = write_data_header(&mut data_file)?;
    Ok((data_path, data_file, header_size))
}

fn data_path_from_base(base_path: impl AsRef<Path>) -> PathBuf {
    base_path.as_ref().with_added_extension("data")
}

/// Writes the file header.
fn write_data_header(file: &mut File) -> Result<usize, WriteDataHeaderError> {
    let header = DataHeaderSerde {
        magic: *DATA_MAGIC,
        major_version: DATA_FORMAT_VERSION.major,
        minor_version: DATA_FORMAT_VERSION.minor,
        patch_version: DATA_FORMAT_VERSION.patch,
        _unused: [0; 24],
    };
    let header_bytes = as_bytes_ref(&header);
    file_io::write_buffer_to_file(file, header_bytes, /*offset*/ 0)?;
    Ok(DATA_HEADER_SIZE)
}

/// Reads and returns the file's header.
pub fn read_data_header(
    file: &File,
    file_len: FileSize,
) -> Result<DataHeader, ReadDataHeaderError> {
    let mut header = DataHeaderSerde {
        magic: [0; 16],
        major_version: 0,
        minor_version: 0,
        patch_version: 0,
        _unused: [0; 24],
    };
    let header_bytes = as_bytes_mut(&mut header);
    let num_bytes_read =
        file_io::read_into_buffer(file, file_len, /*offset*/ 0, header_bytes)?;
    if num_bytes_read != size_of::<DataHeaderSerde>() {
        return Err(ReadDataHeaderError::ShortRead {
            expected: DATA_HEADER_SIZE,
            actual: num_bytes_read,
        });
    }

    if header.magic != *DATA_MAGIC {
        return Err(ReadDataHeaderError::InvalidMagic);
    }
    let format_version = Version::new(
        header.major_version,
        header.minor_version,
        header.patch_version,
    );
    if format_version != DATA_FORMAT_VERSION {
        return Err(ReadDataHeaderError::InvalidFormatVersion(format_version));
    }

    Ok(DataHeader {
        size: DATA_HEADER_SIZE,
        format_version,
    })
}

/// Writes `data` entry to `file` at `offset`.
///
/// - `offset` to write at, does not need to be aligned; the fn will do it.
///   This is to support calling the fn with the current file len as the offset.
///
/// Returns information about the write.
pub fn write_data_entry(
    file: &File,
    offset: FileOffset,
    address: &Pubkey,
    data: &[u8],
) -> Result<WriteInfo, WriteDataEntryError> {
    let data_len = DataLen::try_from(data.len())?;
    let start_offset = offset
        .0
        .checked_next_multiple_of(DATA_ENTRY_OFFSET_ALIGNMENT as u64)
        .ok_or(WriteDataEntryError::OffsetOverrun(offset))?;
    let mut num_bytes_written = 0;

    file_io::write_buffer_to_file(
        file,
        address.as_ref(),
        start_offset + num_bytes_written as FileSize,
    )?;
    num_bytes_written += size_of::<Pubkey>();

    file_io::write_buffer_to_file(
        file,
        data_len.0.to_le_bytes().as_slice(),
        start_offset + num_bytes_written as FileSize,
    )?;
    num_bytes_written += size_of::<DataLen>();

    file_io::write_buffer_to_file(file, data, start_offset + num_bytes_written as u64)?;
    num_bytes_written += data.len();

    let expected_stored_size = calculate_data_entry_stored_size(data.len());
    if num_bytes_written == expected_stored_size {
        Ok(WriteInfo {
            start: FileOffset(start_offset),
            num_bytes_written,
        })
    } else {
        Err(WriteDataEntryError::ShortWrite {
            expected: expected_stored_size,
            actual: num_bytes_written,
        })
    }
}

/// Reads data entry from `file` at `offset`.
///
/// - `file_len` is the size of the file, and bounds reading.
/// - `offset` to read from, must be aligned.
/// - `expected_address` and `expected_data_len` are checked first
///   and must match the data entry before the account data itself is read.
///
/// Returns the account data.
pub fn read_data_entry(
    file: &File,
    file_len: FileSize,
    offset: FileOffset,
    expected_address: &Pubkey,
    expected_data_len: DataLen,
) -> Result<Vec<u8>, ReadDataEntryError> {
    validate_data_entry_offset(file_len, offset, expected_data_len)?;

    let mut data_entry = DataEntrySerde {
        address: Pubkey::default(),
        data_len: 0,
    };
    let data_entry_bytes = as_bytes_mut(&mut data_entry);
    let num_bytes_read = file_io::read_into_buffer(file, file_len, offset.0, data_entry_bytes)
        .map_err(ReadDataEntryError::Io)?;
    if num_bytes_read != size_of::<DataEntrySerde>() {
        return Err(ReadDataEntryError::ShortRead {
            expected: size_of::<DataEntrySerde>(),
            actual: num_bytes_read,
        });
    }

    validate_data_entry(&data_entry, expected_address, expected_data_len)?;

    let data_len = data_entry.data_len as usize;
    let mut buffer = Vec::<u8>::with_capacity(data_len);
    let num_bytes_read = {
        // SAFETY:
        // * buffer is non-null, thus the ptr used for creating the slice is non-null
        // * buffer is at least data_len bytes, thus slice is valid for all bytes
        // * buffer is aligned for account data, thus so is the slice
        // * buffer is a single allocation, thus so is the slice
        // * buffer is not referenced while the slice is alive
        // * buffer's max size is DataLen::MAX, which is less than isize::MAX, thus so is the slice
        let buffer_bytes = unsafe {
            slice::from_raw_parts_mut(
                buffer.spare_capacity_mut().as_mut_ptr().cast::<u8>(),
                data_len,
            )
        };
        file_io::read_into_buffer(
            file,
            file_len,
            offset.0 + DATA_ENTRY_OFFSET_OF_DATA as u64,
            buffer_bytes,
        )
        .map_err(ReadDataEntryError::Io)?
    };
    // SAFETY:
    // * num_bytes_read is <= the buffer's capacity.
    // * only elements 0..num_bytes_read have been written.
    unsafe {
        buffer.set_len(num_bytes_read);
    }

    if num_bytes_read == data_len {
        Ok(buffer)
    } else {
        Err(ReadDataEntryError::ShortRead {
            expected: data_len,
            actual: num_bytes_read,
        })
    }
}

pub fn validate_data_entry_offset(
    file_len: FileSize,
    offset: FileOffset,
    data_len: DataLen,
) -> Result<(), ReadDataEntryError> {
    if !offset.0.is_multiple_of(DATA_ENTRY_OFFSET_ALIGNMENT as u64) {
        return Err(ReadDataEntryError::OffsetUnaligned(offset));
    }

    let stored_size = calculate_data_entry_stored_size(data_len.0 as usize);
    if offset
        .0
        .checked_add(stored_size as u64)
        .is_none_or(|end| end > file_len)
    {
        return Err(ReadDataEntryError::OffsetOverrun(offset));
    }
    Ok(())
}

fn validate_data_entry(
    data_entry: &DataEntrySerde,
    expected_address: &Pubkey,
    expected_data_len: DataLen,
) -> Result<(), ReadDataEntryError> {
    if data_entry.address != *expected_address {
        return Err(ReadDataEntryError::AddressMismatch {
            expected: *expected_address,
            actual: data_entry.address,
        });
    }
    if data_entry.data_len != expected_data_len.0 {
        return Err(ReadDataEntryError::DataLenMismatch {
            expected: expected_data_len.0,
            actual: data_entry.data_len,
        });
    }
    Ok(())
}

/// Parses a complete data entry from a buffered-reader slice and returns the account data without
/// copying it.
pub fn parse_data_entry<'a>(
    bytes: &'a [u8],
    expected_address: &Pubkey,
    expected_data_len: DataLen,
) -> Result<&'a [u8], ReadDataEntryError> {
    let stored_size = calculate_data_entry_stored_size(expected_data_len.0 as usize);
    if bytes.len() < stored_size {
        return Err(ReadDataEntryError::ShortRead {
            expected: stored_size,
            actual: bytes.len(),
        });
    }

    let address_bytes = &bytes[DATA_ENTRY_OFFSET_OF_ADDRESS..][..size_of::<Pubkey>()];
    if address_bytes != expected_address.as_ref() {
        // There is an address mismatch, so now read the stored address and put it in the error.
        // This way we do not copy the stored address out unnecessarily.
        let actual: Pubkey = unsafe { ptr::read_unaligned(address_bytes.as_ptr().cast()) };
        return Err(ReadDataEntryError::AddressMismatch {
            expected: *expected_address,
            actual,
        });
    }

    let data_len_bytes = &bytes[DATA_ENTRY_OFFSET_OF_DATA_LEN..][..size_of::<u32>()];
    let data_len = u32::from_le_bytes(
        data_len_bytes
            .try_into()
            .expect("data_len field has a fixed size"),
    );
    if data_len != expected_data_len.0 {
        return Err(ReadDataEntryError::DataLenMismatch {
            expected: expected_data_len.0,
            actual: data_len,
        });
    }

    Ok(&bytes[DATA_ENTRY_OFFSET_OF_DATA..stored_size])
}

/// Returns the number of bytes required to store a data entry for an account with `data_len`.
pub fn calculate_data_entry_stored_size(data_len: usize) -> usize {
    DATA_ENTRY_FIXED_SIZE + data_len
}

/// On-disk representation of the header.
///
/// Used for writing to/reading from disk.
#[repr(C)]
#[derive(Debug)]
struct DataHeaderSerde {
    magic: [u8; 16],
    major_version: u64,
    minor_version: u64,
    patch_version: u64,
    _unused: [u8; 24],
}
const _: () = const {
    assert!(size_of::<DataHeaderSerde>() == DATA_HEADER_SIZE);
};

// SAFETY: DataHeaderSerde is POD and safe to read/write as bytes
unsafe impl AsBytesRef for DataHeaderSerde {}
unsafe impl AsBytesMut for DataHeaderSerde {}

/// Header for the data file.
#[derive(Debug)]
pub struct DataHeader {
    pub size: usize,
    pub format_version: Version,
}

/// On-disk representation of an entry's fixed portiion.
///
/// Used for writing to/reading from disk.
/// The account data directly follows.
#[repr(C)]
#[derive(Debug)]
struct DataEntrySerde {
    address: Pubkey,
    data_len: u32,
}
const _: () = const {
    // to safely implement AsBytesRef and AsBytesMut, there can be no bytes from padding
    assert!(size_of::<DataEntrySerde>() == size_of::<Pubkey>() + size_of::<u32>());
    assert!(size_of::<DataEntrySerde>() == DATA_ENTRY_FIXED_SIZE);
};

// SAFETY: DataEntrySerde is POD and safe to read/write as bytes
unsafe impl AsBytesRef for DataEntrySerde {}
unsafe impl AsBytesMut for DataEntrySerde {}

#[cfg(test)]
mod tests {
    use {super::*, std::assert_matches, tempfile::TempDir, test_case::test_case};

    fn new_data_entry_bytes(address: &Pubkey, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(DATA_ENTRY_FIXED_SIZE + data.len());
        bytes.extend_from_slice(address.as_ref());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn test_create_data_file() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        _ = create_data_file(&base_path).unwrap();
    }

    #[test]
    fn test_write_and_read_data_header() {
        let mut file = tempfile::tempfile().unwrap();
        let size = write_data_header(&mut file).unwrap();
        let header = read_data_header(&file, size as FileSize).unwrap();
        assert_eq!(header.size, DATA_HEADER_SIZE);
        assert_eq!(header.format_version, DATA_FORMAT_VERSION);
    }

    #[test]
    fn test_read_bad_data_header() {
        let file = tempfile::tempfile().unwrap();

        // test case: truncated header
        {
            let bytes = [0; DATA_HEADER_SIZE - 1];
            file_io::write_buffer_to_file(&file, &bytes, 0).unwrap();
            let err = read_data_header(&file, bytes.len() as FileSize).unwrap_err();
            assert_matches!(err, ReadDataHeaderError::ShortRead { .. });
        }

        // test case: invalid magic
        {
            let header = DataHeaderSerde {
                magic: [0xCD; 16],
                major_version: DATA_FORMAT_VERSION.major,
                minor_version: DATA_FORMAT_VERSION.minor,
                patch_version: DATA_FORMAT_VERSION.patch,
                _unused: [0; 24],
            };
            file_io::write_buffer_to_file(&file, as_bytes_ref(&header), 0).unwrap();
            let err = read_data_header(&file, DATA_HEADER_SIZE as FileSize).unwrap_err();
            assert_matches!(err, ReadDataHeaderError::InvalidMagic);
        }

        // test case: invalid format version
        {
            let header = DataHeaderSerde {
                magic: *DATA_MAGIC,
                major_version: DATA_FORMAT_VERSION.major + 1,
                minor_version: 0,
                patch_version: 0,
                _unused: [0; 24],
            };
            file_io::write_buffer_to_file(&file, as_bytes_ref(&header), 0).unwrap();
            let err = read_data_header(&file, DATA_HEADER_SIZE as FileSize).unwrap_err();
            assert_matches!(err, ReadDataHeaderError::InvalidFormatVersion(_));
        }
    }

    #[test_case(FileSize::MAX, FileOffset(DATA_ENTRY_OFFSET_ALIGNMENT as u64), DataLen(100) => matches Ok(()); "ok")]
    #[test_case(FileSize::MAX, FileOffset(DATA_ENTRY_OFFSET_ALIGNMENT as u64 - 1), DataLen(0) => matches Err(ReadDataEntryError::OffsetUnaligned(_)); "unaligned_minus_1")]
    #[test_case(FileSize::MAX, FileOffset(DATA_ENTRY_OFFSET_ALIGNMENT as u64 + 1), DataLen(0) => matches Err(ReadDataEntryError::OffsetUnaligned(_)); "unaligned_plus_1")]
    #[test_case(FileSize::MAX, FileOffset(FileSize::MAX - DATA_ENTRY_OFFSET_ALIGNMENT as u64 + 1), DataLen(u32::MAX) => matches Err(ReadDataEntryError::OffsetOverrun(_)); "overflow")]
    #[test_case(FileSize::MIN, FileOffset(DATA_ENTRY_OFFSET_ALIGNMENT as u64), DataLen(0) => matches Err(ReadDataEntryError::OffsetOverrun(_)); "overrun")]
    fn test_validate_data_entry_offset(
        file_len: FileSize,
        offset: FileOffset,
        data_len: DataLen,
    ) -> Result<(), ReadDataEntryError> {
        validate_data_entry_offset(file_len, offset, data_len)
    }

    #[test]
    fn test_parse_data_entry_ok() {
        let address = Pubkey::new_unique();
        let account_data = [0xA5; 23];
        let bytes = new_data_entry_bytes(&address, &account_data);
        let parsed = parse_data_entry(
            bytes.as_slice(),
            &address,
            DataLen::try_from(account_data.len()).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed, account_data);
    }

    #[test]
    fn test_parse_data_entry_err_short_read() {
        const DATA_LEN: usize = 6;
        let bytes = [0; DATA_ENTRY_FIXED_SIZE + DATA_LEN - 1];
        let err = parse_data_entry(
            &bytes,
            &Pubkey::default(),
            DataLen::try_from(DATA_LEN).unwrap(),
        )
        .unwrap_err();
        assert_matches!(err, ReadDataEntryError::ShortRead { .. },);
    }

    #[test]
    fn test_parse_data_entry_err_address_mismatch() {
        let address = Pubkey::new_unique();
        let data = [1, 2, 3, 4, 5];
        let bytes = new_data_entry_bytes(&address, &data);
        let data_len = DataLen::try_from(data.len()).unwrap();

        let wrong_address = Pubkey::new_unique();
        let err = parse_data_entry(&bytes, &wrong_address, data_len).unwrap_err();
        assert_matches!(err, ReadDataEntryError::AddressMismatch { .. });
    }

    #[test]
    fn test_parse_data_entry_err_data_len_mismatch() {
        let address = Pubkey::new_unique();
        let data = [1, 2, 3, 4, 5];
        let bytes = new_data_entry_bytes(&address, &data);

        let wrong_data_len = DataLen::try_from(data.len() - 1).unwrap();
        let err = parse_data_entry(&bytes, &address, wrong_data_len).unwrap_err();
        assert_matches!(err, ReadDataEntryError::DataLenMismatch { .. });
    }

    #[test]
    fn test_write_and_read_data_entry() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let (_path, file, size) = create_data_file(&base_path).unwrap();

        let offset = FileOffset(size as FileSize);
        let address = Pubkey::new_unique();
        let data = [0xDA; 123];

        let write_info = write_data_entry(&file, offset, &address, &data).unwrap();
        let end = write_info.start.0 + write_info.num_bytes_written as u64;
        let read_data = read_data_entry(
            &file,
            end,
            write_info.start,
            &address,
            DataLen::try_from(data.len()).unwrap(),
        )
        .unwrap();

        assert_eq!(read_data, data);
    }

    #[test]
    fn test_write_data_entry_bad_offset() {
        let file = tempfile::tempfile().unwrap();

        // offset overflows when aligning
        let err = write_data_entry(&file, FileOffset(FileSize::MAX), &Pubkey::default(), &[])
            .unwrap_err();
        assert_matches!(err, WriteDataEntryError::OffsetOverrun(_));

        // offset overflows when attempting to write the entry
        let err = write_data_entry(
            &file,
            FileOffset(FileSize::MAX - DATA_ENTRY_OFFSET_ALIGNMENT as u64),
            &Pubkey::default(),
            &[],
        )
        .unwrap_err();
        assert_matches!(err, WriteDataEntryError::Io(_));
    }

    #[test]
    fn test_read_data_entry_bad_offset() {
        let file = tempfile::tempfile().unwrap();

        // offset is unaligned
        let err = read_data_entry(
            &file,
            0, // value does not matter
            FileOffset(DATA_ENTRY_OFFSET_ALIGNMENT as u64 + 1),
            &Pubkey::default(), // value does not matter
            DataLen(0),         // value does not matter
        )
        .unwrap_err();
        assert_matches!(err, ReadDataEntryError::OffsetUnaligned(_));

        // offset is past the file len
        let file_len = 11;
        let err = read_data_entry(
            &file,
            file_len,
            FileOffset(file_len.next_multiple_of(DATA_ENTRY_OFFSET_ALIGNMENT as u64)),
            &Pubkey::default(), // value does not matter
            DataLen(0),         // value does not matter
        )
        .unwrap_err();
        assert_matches!(err, ReadDataEntryError::OffsetOverrun(_));
    }
}
