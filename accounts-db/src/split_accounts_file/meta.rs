//! meta file layout:
//!
//!  +-----------+-------------------+---------------------+
//!  |    Offset | Region            |                Size |
//!  +-----------+-------------------+---------------------+
//!  |         0 | meta header       |                64 B |
//!  +-----------+-------------------+---------------------+
//!  |        64 | meta entry 0      | 88 B + DataRef size |
//!  +-----------+-------------------+---------------------+
//!  |           | alignment padding | to 32-byte boundary |
//!  +-----------+-------------------+---------------------+
//!  | align(32) | meta entry 1      | 88 B + DataRef size |
//!  +-----------+-------------------+---------------------+
//!  |           | alignment padding | to 32-byte boundary |
//!  +-----------+-------------------+---------------------+
//!  | align(32) | meta entry 2      | 88 B + DataRef size |
//!  +-----------+-------------------+---------------------+
//!  |           | ...               |                     |
//!  +-----------+-------------------+---------------------+
//!
//!  - meta entries are written at file offsets aligned to 32
//!
//! meta header layout:
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
//! meta entry layout:
//!
//!  +--------+---------------+---------+--------+
//!  | Offset | Field         | Type    |   Size |
//!  +--------+---------------+---------+--------+
//!  |      0 | address       | Pubkey  |   32 B |
//!  +--------+---------------+---------+--------+
//!  |     32 | owner         | Pubkey  |   32 B |
//!  +--------+---------------+---------+--------+
//!  |     64 | lamports      | u64     |    8 B |
//!  +--------+---------------+---------+--------+
//!  |     72 | rent_epoch    | u64     |    8 B |
//!  +--------+---------------+---------+--------+
//!  |     80 | is_executable | u8      |    1 B |
//!  +--------+---------------+---------+--------+
//!  |     81 | reserved      | [u8; 3] |    3 B |
//!  +--------+---------------+---------+--------+
//!  |     84 | data_len      | u32     |    4 B |
//!  +--------+---------------+---------+--------+
//!  |     88 | data_ref      | DataRef | varies |
//!  +--------+---------------+---------+--------+
//!
//!  - fixed size: 88 B
//!  - max size: 4096 B
//!
//! data ref layout:
//!
//!  +--------+--------------+----------------+----------+
//!  | Offset | Union Member | Type           |     Size |
//!  +--------+--------------+----------------+----------+
//!  |      0 | NoData       | ()             |      0 B |
//!  +--------+--------------+----------------+----------+
//!  |      0 | Inline       | [u8; data_len] | data_len |
//!  +--------+--------------+----------------+----------+
//!  |      0 | External     | u64            |      8 B |
//!  +--------+--------------+----------------+----------+
//!
//!  - DataRef is an untagged union. The active member is inferred from data_len.
//!    ```text
//!    union DataRef {
//!        NoData,
//!        Inline { data: [u8; data_len] },
//!        External { data_file_offset: u64 },
//!    }
//!    ```
//!    data_len == 0          -> NoData
//!    1 <= data_len <= 4008  -> Inline
//!    data_len > 4008        -> External

use {
    super::{
        DataLen, DataRefBorrowed, SplitAccountsFileError,
        as_bytes::{AsBytesMut, AsBytesRef, as_bytes_mut, as_bytes_ref},
        common::{ExternalDataOffset, FileOffset, WriteInfo},
        error::{
            ReadMetaEntryError, ReadMetaHeaderError, WriteMetaEntryError, WriteMetaHeaderError,
        },
        utils,
    },
    agave_fs::{FileSize, file_io},
    semver::Version,
    solana_pubkey::Pubkey,
    std::{
        cmp,
        convert::TryFrom,
        fs::File,
        mem::{MaybeUninit, offset_of},
        path::{Path, PathBuf},
        ptr, slice,
    },
};

pub const META_HEADER_SIZE: usize = 64;

const META_MAGIC: &[u8; 16] = b"agave meta file\0";
const META_FORMAT_VERSION: Version = Version::new(0, 1, 0);

/// meta entries are always written at offsets that are multiples of this alignment
pub const META_ENTRY_OFFSET_ALIGNMENT: usize = 1 << META_ENTRY_OFFSET_ALIGNMENT_LOG2;
pub const META_ENTRY_OFFSET_ALIGNMENT_LOG2: u32 = 5;

/// Maximum size, in bytes, of a meta entry.
pub const META_ENTRY_MAX_SIZE: usize = 4 * 1024;

/// Maximum size, in bytes, for account data that will be stored inline in a meta entry.
pub const META_ENTRY_INLINE_DATA_MAX_SIZE: usize = META_ENTRY_MAX_SIZE - META_ENTRY_FIXED_SIZE;

/// Size, in bytes, of a meta entry's fixed portion (i.e. *not* the data ref).
pub const META_ENTRY_FIXED_SIZE: usize = 88;

// byte-offsets of the fields within a meta entry
const META_ENTRY_OFFSET_OF_ADDRESS: usize = 0;
const META_ENTRY_OFFSET_OF_OWNER: usize = 32;
const META_ENTRY_OFFSET_OF_LAMPORTS: usize = 64;
const META_ENTRY_OFFSET_OF_RENT_EPOCH: usize = 72;
const META_ENTRY_OFFSET_OF_IS_EXECUTABLE: usize = 80;
const META_ENTRY_OFFSET_OF_DATA_LEN: usize = 84;
const META_ENTRY_OFFSET_OF_DATA_REF: usize = 88;

pub fn create_meta_file(
    base_path: impl AsRef<Path>,
) -> Result<(PathBuf, File, usize), SplitAccountsFileError> {
    let meta_path = meta_path_from_base(&base_path);
    let mut meta_file = utils::create_new_file(&meta_path)?;
    let header_size = write_meta_header(&mut meta_file)?;
    Ok((meta_path, meta_file, header_size))
}

fn meta_path_from_base(base_path: impl AsRef<Path>) -> PathBuf {
    base_path.as_ref().with_added_extension("meta")
}

fn write_meta_header(file: &mut File) -> Result<usize, WriteMetaHeaderError> {
    let header = MetaHeaderSerde {
        magic: *META_MAGIC,
        major_version: META_FORMAT_VERSION.major,
        minor_version: META_FORMAT_VERSION.minor,
        patch_version: META_FORMAT_VERSION.patch,
        _unused: [0; 24],
    };
    let header_bytes = as_bytes_ref(&header);
    file_io::write_buffer_to_file(file, header_bytes, /*offset*/ 0)?;
    Ok(META_HEADER_SIZE)
}

pub fn read_meta_header(
    file: &File,
    file_len: FileSize,
) -> Result<MetaHeader, ReadMetaHeaderError> {
    let mut header = MetaHeaderSerde {
        magic: [0; 16],
        major_version: 0,
        minor_version: 0,
        patch_version: 0,
        _unused: [0; 24],
    };
    let header_bytes = as_bytes_mut(&mut header);
    let num_bytes_read =
        file_io::read_into_buffer(file, file_len, /*offset*/ 0, header_bytes)?;
    if num_bytes_read != META_HEADER_SIZE {
        return Err(ReadMetaHeaderError::ShortRead {
            expected: META_HEADER_SIZE,
            actual: num_bytes_read,
        });
    }

    if header.magic != *META_MAGIC {
        return Err(ReadMetaHeaderError::InvalidMagic);
    }
    let format_version = Version::new(
        header.major_version,
        header.minor_version,
        header.patch_version,
    );
    if format_version != META_FORMAT_VERSION {
        return Err(ReadMetaHeaderError::InvalidFormatVersion(format_version));
    }

    Ok(MetaHeader {
        size: META_HEADER_SIZE,
        format_version,
    })
}

/// Writes a meta entry to `file` at `offset`.
///
/// Returns a `WriteInfo` that contains information about what was written.
pub fn write_meta_entry(
    file: &File,
    offset: FileOffset,
    address: &Pubkey,
    owner: &Pubkey,
    lamports: u64,
    rent_epoch: u64,
    is_executable: bool,
    data_len: DataLen,
    data_ref: DataRefBorrowed,
) -> Result<WriteInfo, WriteMetaEntryError> {
    match data_ref {
        DataRefBorrowed::NoData => {
            debug_assert_eq!(data_len.0, 0);
        }
        DataRefBorrowed::Inline(data) => {
            debug_assert_eq!(data_len.0 as usize, data.len());
        }
        DataRefBorrowed::External(_) => {
            // nothing to assert
        }
    };

    let start_offset = offset
        .0
        .checked_next_multiple_of(META_ENTRY_OFFSET_ALIGNMENT as u64)
        .ok_or(WriteMetaEntryError::OffsetOverrun(offset))?;
    let mut num_bytes_written = 0;

    let meta_entry = MetaEntrySerde {
        address: *address,
        owner: *owner,
        lamports,
        rent_epoch,
        is_executable: u8::from(is_executable),
        _unused: [0; 3],
        data_len: data_len.0,
    };
    let meta_entry_bytes = as_bytes_ref(&meta_entry);
    file_io::write_buffer_to_file(
        file,
        meta_entry_bytes,
        start_offset + num_bytes_written as u64,
    )?;
    num_bytes_written += size_of::<MetaEntrySerde>();

    match data_ref {
        DataRefBorrowed::NoData => {
            // no data, so nothing to write
        }
        DataRefBorrowed::Inline(data) => {
            // write `data` inline here
            file_io::write_buffer_to_file(file, data, start_offset + num_bytes_written as u64)?;
            num_bytes_written += data.len();
        }
        DataRefBorrowed::External(external_data_offset) => {
            // data was written to external data file, so only write the offset here
            file_io::write_buffer_to_file(
                file,
                external_data_offset.0.0.to_le_bytes().as_slice(),
                start_offset + num_bytes_written as u64,
            )?;
            num_bytes_written += size_of_val(&external_data_offset);
        }
    }

    let expected_stored_size = calculate_meta_entry_stored_size(data_len);
    if num_bytes_written == expected_stored_size {
        Ok(WriteInfo {
            start: FileOffset(start_offset),
            num_bytes_written,
        })
    } else {
        Err(WriteMetaEntryError::ShortWrite {
            expected: expected_stored_size,
            actual: num_bytes_written,
        })
    }
}

/// Reads meta entry from `file` at `offset` and then calls `callback` with it.
pub fn read_meta_entry<Ret>(
    file: &File,
    file_len: FileSize,
    offset: FileOffset,
    callback: impl for<'local> FnOnce(MetaEntryRef<'local>, DataRefBorrowed<'local>) -> Ret,
) -> Result<Ret, ReadMetaEntryError> {
    validate_meta_entry_offset(file_len, offset)?;

    let read_size = cmp::min(META_ENTRY_MAX_SIZE, (file_len - offset.0) as usize);
    let mut buffer = MetaEntryBuffer([MaybeUninit::uninit(); META_ENTRY_MAX_SIZE]);
    let valid_bytes = {
        // SAFETY:
        // * buffer is non-null, thus the ptr used for creating the slice is non-null
        // * buffer is at least read_size bytes, thus slice is valid for all bytes
        // * buffer is aligned for a meta entry, thus so is the slice
        // * buffer is a single allocation, thus so is the slice
        // * buffer is not referenced while the slice is alive
        // * buffer is META_ENTRY_MAX_SIZE, which is less than isize::MAX, thus so is the slice
        let buffer_bytes =
            unsafe { slice::from_raw_parts_mut(buffer.0.as_mut_ptr().cast::<u8>(), read_size) };
        let num_bytes_read = file_io::read_into_buffer(file, file_len, offset.0, buffer_bytes)?;
        &buffer_bytes[..num_bytes_read]
    };
    let meta_entry = parse_meta_entry_fixed(valid_bytes)?;
    let data_ref = parse_meta_entry_data_ref(valid_bytes, &meta_entry)?;
    Ok(callback(meta_entry, data_ref))
}

/// Parses the byte-slice, `bytes`, and returns a `MetaEntryRef` if is it valid.
pub fn parse_meta_entry_fixed(bytes: &[u8]) -> Result<MetaEntryRef<'_>, ReadMetaEntryError> {
    if bytes.len() < META_ENTRY_FIXED_SIZE {
        return Err(ReadMetaEntryError::ShortRead {
            expected: META_ENTRY_FIXED_SIZE,
            actual: bytes.len(),
        });
    }

    let ptr = bytes.as_ptr();

    // SAFETY: For address and owner:
    // * all byte patterns are valid
    // * Pubkey has an alignment of 1, so reading from ptr is also aligned
    // * they are tied to the lifetime of `bytes` and thus do not dangle
    let address = unsafe { &*ptr.add(META_ENTRY_OFFSET_OF_ADDRESS).cast() };
    let owner = unsafe { &*ptr.add(META_ENTRY_OFFSET_OF_OWNER).cast() };

    // SAFETY: For these fields:
    // * all byte patterns are valid
    // * we are reading from pointers that point to valid type T's
    let lamports: u64 =
        unsafe { ptr::read_unaligned(ptr.add(META_ENTRY_OFFSET_OF_LAMPORTS).cast()) };
    let rent_epoch: u64 =
        unsafe { ptr::read_unaligned(ptr.add(META_ENTRY_OFFSET_OF_RENT_EPOCH).cast()) };
    let is_executable_raw: u8 = bytes[META_ENTRY_OFFSET_OF_IS_EXECUTABLE];
    let data_len_raw: u32 =
        unsafe { ptr::read_unaligned(ptr.add(META_ENTRY_OFFSET_OF_DATA_LEN).cast()) };

    // Validate `is_executable` and `data_len` to ensure their bits are valid.
    let is_executable = bool::try_from(is_executable_raw)
        .map_err(|_| ReadMetaEntryError::InvalidIsExecutable(is_executable_raw))?;
    let data_len = DataLen::try_from(data_len_raw as usize)?;

    Ok(MetaEntryRef {
        address,
        owner,
        lamports,
        rent_epoch,
        is_executable,
        data_len,
    })
}

/// Parses the variable portion of a meta entry and returns a reference to inline account data.
pub fn parse_meta_entry_data_ref<'a>(
    bytes: &'a [u8],
    meta_entry: &MetaEntryRef<'_>,
) -> Result<DataRefBorrowed<'a>, ReadMetaEntryError> {
    let stored_size = calculate_meta_entry_stored_size(meta_entry.data_len);
    if bytes.len() < stored_size {
        return Err(ReadMetaEntryError::ShortRead {
            expected: stored_size,
            actual: bytes.len(),
        });
    }

    if meta_entry.data_len.0 == 0 {
        Ok(DataRefBorrowed::NoData)
    } else if should_store_account_data_in_meta_file(meta_entry.data_len) {
        Ok(DataRefBorrowed::Inline(
            &bytes[META_ENTRY_OFFSET_OF_DATA_REF..stored_size],
        ))
    } else {
        let external_offset_bytes = bytes[META_ENTRY_OFFSET_OF_DATA_REF..stored_size]
            .try_into()
            .expect("external data offset has a fixed size");
        Ok(DataRefBorrowed::External(ExternalDataOffset(FileOffset(
            u64::from_le_bytes(external_offset_bytes),
        ))))
    }
}

fn validate_meta_entry_offset(
    file_len: FileSize,
    offset: FileOffset,
) -> Result<(), ReadMetaEntryError> {
    if !offset.0.is_multiple_of(META_ENTRY_OFFSET_ALIGNMENT as u64) {
        return Err(ReadMetaEntryError::OffsetUnaligned(offset));
    }
    if offset
        .0
        .checked_add(META_ENTRY_FIXED_SIZE as u64)
        .is_none_or(|end| end > file_len)
    {
        return Err(ReadMetaEntryError::OffsetOverrun(offset));
    }
    Ok(())
}

/// Returns the number of bytes required to store a meta entry for an account with `data_len`.
pub fn calculate_meta_entry_stored_size(data_len: DataLen) -> usize {
    META_ENTRY_FIXED_SIZE
        + if should_store_account_data_in_meta_file(data_len) {
            data_len.0 as usize
        } else {
            size_of::<ExternalDataOffset>()
        }
}

/// Returns if an account with `data_len` should store its data inline in the meta entry or not.
pub fn should_store_account_data_in_meta_file(data_len: DataLen) -> bool {
    data_len.0 as usize <= META_ENTRY_INLINE_DATA_MAX_SIZE
}

#[repr(C)]
#[derive(Debug)]
struct MetaHeaderSerde {
    magic: [u8; 16],
    major_version: u64,
    minor_version: u64,
    patch_version: u64,
    _unused: [u8; 24],
}
const _: () = const {
    assert!(size_of::<MetaHeaderSerde>() == META_HEADER_SIZE);
    assert!(size_of::<MetaHeaderSerde>().is_multiple_of(META_ENTRY_OFFSET_ALIGNMENT));
};

/// SAFETY: MetaHeaderSerde is POD and safe to read/write as bytes
unsafe impl AsBytesRef for MetaHeaderSerde {}
unsafe impl AsBytesMut for MetaHeaderSerde {}

/// Header for a meta file.
#[derive(Debug)]
pub struct MetaHeader {
    pub size: usize,
    pub format_version: Version,
}

/// The fixed portion of a meta entry (everything except the data ref).
///
/// Used for writing-to and reading-from disk.
#[repr(C)]
#[derive(Debug)]
struct MetaEntrySerde {
    address: Pubkey,
    owner: Pubkey,
    lamports: u64,
    rent_epoch: u64,
    is_executable: u8,
    _unused: [u8; 3],
    data_len: u32,
}
const _: () = const {
    // to safely implement BytesRef and BytesMut, there can be no bytes from padding
    assert!(
        size_of::<MetaEntrySerde>()
            == size_of::<Pubkey>() /*address*/
        + size_of::<Pubkey>() /*owner*/
        + size_of::<u64>() /*lamports*/
        + size_of::<u64>() /*rent_epoch*/
        + size_of::<u8>() /*is_executable*/
        + size_of::<[u8; 3]>() /*_unused*/
        + size_of::<u32>() /*data_len*/
    );
    assert!(size_of::<MetaEntrySerde>() == META_ENTRY_FIXED_SIZE);
    assert!(offset_of!(MetaEntrySerde, address) == META_ENTRY_OFFSET_OF_ADDRESS);
    assert!(offset_of!(MetaEntrySerde, owner) == META_ENTRY_OFFSET_OF_OWNER);
    assert!(offset_of!(MetaEntrySerde, lamports) == META_ENTRY_OFFSET_OF_LAMPORTS);
    assert!(offset_of!(MetaEntrySerde, rent_epoch) == META_ENTRY_OFFSET_OF_RENT_EPOCH);
    assert!(offset_of!(MetaEntrySerde, is_executable) == META_ENTRY_OFFSET_OF_IS_EXECUTABLE);
    assert!(offset_of!(MetaEntrySerde, data_len) == META_ENTRY_OFFSET_OF_DATA_LEN);
};

// SAFETY: MetaEntrySerde is POD and safe to read/write as bytes
unsafe impl AsBytesRef for MetaEntrySerde {}
unsafe impl AsBytesMut for MetaEntrySerde {}

/// Borrowed view of a meta entry.
///
/// Used for reading, so pubkey fields do not need to be copied.
#[derive(Debug)]
pub struct MetaEntryRef<'a> {
    pub address: &'a Pubkey,
    pub owner: &'a Pubkey,
    pub lamports: u64,
    pub rent_epoch: u64,
    pub is_executable: bool,
    pub data_len: DataLen,
}

/// Byte buffer used for reading meta entries.
///
/// This buffer is aligned to ensure it is safe to read
/// the meta entry fields after loading from disk.
#[repr(align(8))]
struct MetaEntryBuffer([MaybeUninit<u8>; META_ENTRY_MAX_SIZE]);
const _: () = const {
    assert!(align_of::<MetaEntryBuffer>() == align_of::<MetaEntrySerde>());
};

#[cfg(test)]
mod tests {
    use {super::*, std::assert_matches, tempfile::TempDir, test_case::test_case};

    #[test]
    fn test_create_meta_file() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        _ = create_meta_file(&base_path).unwrap();
    }

    #[test]
    fn test_write_and_read_meta_header() {
        let mut file = tempfile::tempfile().unwrap();
        let size = write_meta_header(&mut file).unwrap();
        let header = read_meta_header(&file, size as FileSize).unwrap();
        assert_eq!(header.size, META_HEADER_SIZE);
        assert_eq!(header.format_version, META_FORMAT_VERSION);
    }

    #[test]
    fn test_read_bad_meta_header() {
        let file = tempfile::tempfile().unwrap();

        // test case: truncated header
        {
            let bytes = [0; META_HEADER_SIZE - 1];
            file_io::write_buffer_to_file(&file, &bytes, 0).unwrap();
            let err = read_meta_header(&file, bytes.len() as FileSize).unwrap_err();
            assert_matches!(err, ReadMetaHeaderError::ShortRead { .. });
        }

        // test case: invalid magic
        {
            let header = MetaHeaderSerde {
                magic: [0xBA; 16],
                major_version: META_FORMAT_VERSION.major,
                minor_version: META_FORMAT_VERSION.minor,
                patch_version: META_FORMAT_VERSION.patch,
                _unused: [0; 24],
            };
            file_io::write_buffer_to_file(&file, as_bytes_ref(&header), 0).unwrap();
            let err = read_meta_header(&file, META_HEADER_SIZE as FileSize).unwrap_err();
            assert_matches!(err, ReadMetaHeaderError::InvalidMagic);
        }

        // test case: invalid format version
        {
            let header = MetaHeaderSerde {
                magic: *META_MAGIC,
                major_version: META_FORMAT_VERSION.major + 1,
                minor_version: 0,
                patch_version: 0,
                _unused: [0; 24],
            };
            file_io::write_buffer_to_file(&file, as_bytes_ref(&header), 0).unwrap();
            let err = read_meta_header(&file, META_HEADER_SIZE as FileSize).unwrap_err();
            assert_matches!(err, ReadMetaHeaderError::InvalidFormatVersion(_));
        }
    }

    #[test_case(FileSize::MAX, FileOffset(META_ENTRY_OFFSET_ALIGNMENT as u64) => matches Ok(()); "ok")]
    #[test_case(FileSize::MAX, FileOffset(META_ENTRY_OFFSET_ALIGNMENT as u64 - 1) => matches Err(ReadMetaEntryError::OffsetUnaligned(_)); "unaligned_minus_1")]
    #[test_case(FileSize::MAX, FileOffset(META_ENTRY_OFFSET_ALIGNMENT as u64 + 1) => matches Err(ReadMetaEntryError::OffsetUnaligned(_)); "unaligned_plus_1")]
    #[test_case(FileSize::MAX, FileOffset(FileSize::MAX - META_ENTRY_OFFSET_ALIGNMENT as u64 + 1) => matches Err(ReadMetaEntryError::OffsetOverrun(_)); "overflow")]
    #[test_case(FileSize::MIN, FileOffset(META_ENTRY_OFFSET_ALIGNMENT as u64) => matches Err(ReadMetaEntryError::OffsetOverrun(_)); "overrun")]
    fn test_validate_meta_entry_offset(
        file_len: FileSize,
        offset: FileOffset,
    ) -> Result<(), ReadMetaEntryError> {
        validate_meta_entry_offset(file_len, offset)
    }

    #[test]
    fn test_parse_meta_entry_fixed_ok() {
        let meta_entry = MetaEntrySerde {
            address: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            lamports: 123,
            rent_epoch: 456,
            is_executable: u8::from(true),
            _unused: [0; 3],
            data_len: 789,
        };
        let parsed = parse_meta_entry_fixed(as_bytes_ref(&meta_entry)).unwrap();
        assert_eq!(parsed.address, &meta_entry.address);
        assert_eq!(parsed.owner, &meta_entry.owner);
        assert_eq!(parsed.lamports, meta_entry.lamports);
        assert_eq!(parsed.rent_epoch, meta_entry.rent_epoch);
        assert_eq!(u8::from(parsed.is_executable), meta_entry.is_executable);
        assert_eq!(parsed.data_len.0, meta_entry.data_len);
    }

    #[test]
    fn test_parse_meta_entry_fixed_err_short_read() {
        let bytes = [0; META_ENTRY_FIXED_SIZE - 1];
        let err = parse_meta_entry_fixed(&bytes).unwrap_err();
        assert_matches!(err, ReadMetaEntryError::ShortRead { .. },);
    }

    #[test]
    fn test_parse_meta_entry_fixed_err_invalid_is_executable() {
        let meta_entry = MetaEntrySerde {
            address: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            lamports: 123,
            rent_epoch: 456,
            is_executable: 7,
            _unused: [0; 3],
            data_len: 0,
        };
        let err = parse_meta_entry_fixed(as_bytes_ref(&meta_entry)).unwrap_err();
        assert_matches!(err, ReadMetaEntryError::InvalidIsExecutable(_));
    }

    #[test]
    fn test_parse_meta_entry_fixed_err_invalid_data_len() {
        let meta_entry = MetaEntrySerde {
            address: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            lamports: 123,
            rent_epoch: 456,
            is_executable: 0,
            _unused: [0; 3],
            data_len: DataLen::MAX + 1,
        };
        let err = parse_meta_entry_fixed(as_bytes_ref(&meta_entry)).unwrap_err();
        assert_matches!(err, ReadMetaEntryError::InvalidDataLen(_));
    }

    #[test]
    fn test_parse_meta_entry_data_ref_err_short_read() {
        const DATA_LEN: usize = 7;
        let meta_entry = MetaEntryRef {
            address: &Pubkey::default(),
            owner: &Pubkey::default(),
            lamports: 123,
            rent_epoch: u64::MAX,
            is_executable: false,
            data_len: DataLen::try_from(DATA_LEN).unwrap(),
        };
        let bytes = [0; DATA_LEN - 1];
        let err = parse_meta_entry_data_ref(&bytes, &meta_entry).unwrap_err();
        assert_matches!(err, ReadMetaEntryError::ShortRead { .. });
    }

    #[test]
    fn test_write_and_read_meta_entry() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let (_path, file, size) = create_meta_file(&base_path).unwrap();

        let mut offset = FileOffset(size as FileSize);

        // no data
        {
            let address = Pubkey::new_unique();
            let owner = Pubkey::new_unique();
            let lamports = 1_000_000;
            let rent_epoch = u64::MAX;
            let is_executable = false;
            let data_len = DataLen::try_from(0).unwrap();
            let data_ref = DataRefBorrowed::NoData;
            let write_info = write_meta_entry(
                &file,
                offset,
                &address,
                &owner,
                lamports,
                rent_epoch,
                is_executable,
                data_len,
                data_ref,
            )
            .unwrap();

            let end = offset.0 + write_info.num_bytes_written as u64;
            read_meta_entry(&file, end, offset, |read_meta_entry, read_data_ref| {
                assert_eq!(read_meta_entry.address, &address);
                assert_eq!(read_meta_entry.owner, &owner);
                assert_eq!(read_meta_entry.lamports, lamports);
                assert_eq!(read_meta_entry.rent_epoch, rent_epoch);
                assert_eq!(read_meta_entry.is_executable, is_executable);
                assert_eq!(read_meta_entry.data_len, data_len);
                assert_eq!(read_data_ref, DataRefBorrowed::NoData);
            })
            .unwrap();

            offset.0 = end.next_multiple_of(META_ENTRY_OFFSET_ALIGNMENT as u64);
        }

        // inline data
        {
            let address = Pubkey::new_unique();
            let owner = Pubkey::new_unique();
            let lamports = 1_000_200;
            let rent_epoch = u64::MAX;
            let is_executable = false;
            let data = [0xAB; 200];
            let data_len = DataLen::try_from(data.len()).unwrap();
            let data_ref = DataRefBorrowed::Inline(&data);
            let write_info = write_meta_entry(
                &file,
                offset,
                &address,
                &owner,
                lamports,
                rent_epoch,
                is_executable,
                data_len,
                data_ref,
            )
            .unwrap();

            let end = offset.0 + write_info.num_bytes_written as u64;
            read_meta_entry(&file, end, offset, |read_meta_entry, read_data_ref| {
                assert_eq!(read_meta_entry.address, &address);
                assert_eq!(read_meta_entry.owner, &owner);
                assert_eq!(read_meta_entry.lamports, lamports);
                assert_eq!(read_meta_entry.rent_epoch, rent_epoch);
                assert_eq!(read_meta_entry.is_executable, is_executable);
                assert_eq!(read_meta_entry.data_len, data_len);
                assert_eq!(read_data_ref, DataRefBorrowed::Inline(&data));
            })
            .unwrap();

            offset.0 = end.next_multiple_of(META_ENTRY_OFFSET_ALIGNMENT as u64);
        }

        // external data
        {
            let address = Pubkey::new_unique();
            let owner = Pubkey::new_unique();
            let lamports = 1_234_567;
            let rent_epoch = u64::MAX;
            let is_executable = false;
            let data_len = DataLen::try_from(META_ENTRY_MAX_SIZE + 1).unwrap();
            let external_data_offset = ExternalDataOffset(FileOffset(234_567));
            let data_ref = DataRefBorrowed::External(external_data_offset);
            let write_info = write_meta_entry(
                &file,
                offset,
                &address,
                &owner,
                lamports,
                rent_epoch,
                is_executable,
                data_len,
                data_ref,
            )
            .unwrap();

            let end = offset.0 + write_info.num_bytes_written as u64;
            read_meta_entry(&file, end, offset, |read_meta_entry, read_data_ref| {
                assert_eq!(read_meta_entry.address, &address);
                assert_eq!(read_meta_entry.owner, &owner);
                assert_eq!(read_meta_entry.lamports, lamports);
                assert_eq!(read_meta_entry.rent_epoch, rent_epoch);
                assert_eq!(read_meta_entry.is_executable, is_executable);
                assert_eq!(read_meta_entry.data_len, data_len);
                assert_eq!(
                    read_data_ref,
                    DataRefBorrowed::External(external_data_offset),
                );
            })
            .unwrap();
        }
    }

    #[test]
    fn test_write_meta_entry_bad_offset() {
        let file = tempfile::tempfile().unwrap();

        // offset overflows when aligning
        let err = write_meta_entry(
            &file,
            FileOffset(FileSize::MAX),
            &Pubkey::default(),
            &Pubkey::default(),
            123,
            0,
            false,
            DataLen(0),
            DataRefBorrowed::NoData,
        )
        .unwrap_err();
        assert_matches!(err, WriteMetaEntryError::OffsetOverrun(_));

        // offset overflows when attempting to write the entry
        let err = write_meta_entry(
            &file,
            FileOffset(FileSize::MAX - META_ENTRY_OFFSET_ALIGNMENT as u64),
            &Pubkey::default(),
            &Pubkey::default(),
            123,
            0,
            false,
            DataLen(0),
            DataRefBorrowed::NoData,
        )
        .unwrap_err();
        assert_matches!(err, WriteMetaEntryError::Io(_));
    }

    #[test]
    fn test_read_meta_entry_bad_offset() {
        let file = tempfile::tempfile().unwrap();

        // offset is unaligned
        let err = read_meta_entry(
            &file,
            0, // value does not matter
            FileOffset(META_ENTRY_OFFSET_ALIGNMENT as u64 + 1),
            |_, _| (), // does not matter
        )
        .unwrap_err();
        assert_matches!(err, ReadMetaEntryError::OffsetUnaligned(_));

        // offset is past the file len
        let file_len = 11;
        let err = read_meta_entry(
            &file,
            file_len,
            FileOffset(file_len.next_multiple_of(META_ENTRY_OFFSET_ALIGNMENT as u64)),
            |_, _| (), // does not matter
        )
        .unwrap_err();
        assert_matches!(err, ReadMetaEntryError::OffsetOverrun(_));
    }
}
