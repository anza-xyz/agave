#![allow(dead_code)]
mod as_bytes;
mod common;
mod data;
mod error;
mod meta;
mod utils;

pub use error::Error as SplitAccountsFileError;
use {
    self::{
        common::{DataLen, DataRefBorrowed, ExternalDataOffset, FileOffset, LogicalOffset},
        data::{
            DATA_ENTRY_FIXED_SIZE, calculate_data_entry_stored_size, create_data_file,
            parse_data_entry, read_data_entry, read_data_header, validate_data_entry_offset,
            write_data_entry,
        },
        meta::{
            META_ENTRY_FIXED_SIZE, META_ENTRY_OFFSET_ALIGNMENT, META_HEADER_SIZE, MetaEntryRef,
            calculate_meta_entry_stored_size, create_meta_file, parse_meta_entry_data_ref,
            parse_meta_entry_fixed, read_meta_entry, read_meta_header,
            should_store_account_data_in_meta_file, write_meta_entry,
        },
        utils::{file_offset_from_logical, logical_offset_from_file},
    },
    crate::{
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        storable_accounts::StorableAccounts,
        utils::create_account_shared_data,
    },
    agave_fs::{
        FileInfo, FileSize,
        buffered_reader::{
            BufReaderWithOverflow, BufferedReader, FileBufRead as _, RequiredLenBufFileRead,
            RequiredLenBufRead as _,
        },
    },
    solana_account::{AccountSharedData, ReadableAccount},
    std::{
        convert::TryFrom,
        fs::{self, File},
        path::{Path, PathBuf},
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    },
};

pub static SPLIT_ACCOUNTS_FILE_STATS: SplitAccountsFileStats = SplitAccountsFileStats {
    num_open: AtomicU64::new(0),
    num_dirty: AtomicU64::new(0),
};

/// Buffer size to use when scaning a meta file.
const META_SCAN_BUFFER_SIZE: usize = 16 * 1024;

/// Creates a reusable buffered reader tuned for scanning external account data.
pub fn new_scan_accounts_reader<'a>() -> impl RequiredLenBufFileRead<'a> {
    // 128KiB covers a reasonably large distribution of typical account sizes.
    // In a recent sample, 99.98% of accounts' data lengths were less than or equal to 128KiB.
    const MIN_CAPACITY: usize = 128 * 1024;
    const MAX_CAPACITY: usize = DATA_ENTRY_FIXED_SIZE + DataLen::MAX as usize;
    const BUFFER_SIZE: usize = 32 * 1024;
    BufReaderWithOverflow::new(
        BufferedReader::<BUFFER_SIZE>::new(),
        MIN_CAPACITY,
        MAX_CAPACITY,
    )
}

/// Account storage backed by split metadata and data files.
///
/// Account index offsets point into the meta file. Small account data is stored
/// inline in the meta entry; larger data is stored in the sibling data file.
#[derive(Debug)]
pub struct SplitAccountsFile {
    meta_path: PathBuf,
    data_path: PathBuf,

    /// Flags if the file is dirty or not.
    /// Since fastboot requires that all storages are flushed to disk, be smart about it.
    /// Accounts files are (almost) always write-once.
    /// This avoids unnecessary syscalls/kernel work when nothing in the file has changed.
    is_dirty: AtomicBool,

    /// if true, remove meta and data files when dropped
    remove_on_drop: AtomicBool,

    /// inner state, used to distinguish read-only vs writable
    inner: InnerState,
}

impl Drop for SplitAccountsFile {
    fn drop(&mut self) {
        SPLIT_ACCOUNTS_FILE_STATS
            .num_open
            .fetch_sub(1, Ordering::Relaxed);

        if *self.is_dirty.get_mut() {
            SPLIT_ACCOUNTS_FILE_STATS
                .num_dirty
                .fetch_sub(1, Ordering::Relaxed);
        }

        if *self.remove_on_drop.get_mut() {
            if let Err(err) = fs::remove_file(&self.meta_path) {
                log::warn!(
                    "SplitAccountsFile::drop() failed to remove '{}': {err}",
                    self.meta_path.display(),
                );
            }
            if let Err(err) = fs::remove_file(&self.data_path) {
                log::warn!(
                    "SplitAccountsFile::drop() failed to remove '{}': {err}",
                    self.data_path.display(),
                );
            }
        }
    }
}

impl SplitAccountsFile {
    /// Instantiates a SplitAccountsFile, creating new meta and data files from `base_path`.
    ///
    /// `base_path` is _not_ supposed to be a directory, but rather a file name.
    /// E.g. <accounts-dir>/SLOT.ID, just like an AppendVec file name.
    pub fn new(base_path: impl AsRef<Path>) -> Result<Self, SplitAccountsFileError> {
        let (meta_path, meta_file, meta_len) = create_meta_file(&base_path)?;
        let (data_path, data_file, data_len) = create_data_file(&base_path)?;
        SPLIT_ACCOUNTS_FILE_STATS
            .num_open
            .fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            meta_path,
            data_path,
            is_dirty: AtomicBool::new(false),
            remove_on_drop: AtomicBool::new(true),
            inner: InnerState::Writable(WritableState {
                write_lock: Mutex::new(()),
                meta_file,
                meta_len: AtomicU64::new(meta_len as u64),
                data_file,
                data_len: AtomicU64::new(data_len as u64),
            }),
        })
    }

    /// Instantiates a SplitAccountsFile, opening preexisting `meta` and `data.
    pub fn open(meta: FileInfo, data: FileInfo) -> Result<Self, SplitAccountsFileError> {
        // read meta header, ensure header len and file len match
        // read data header, ensure header len and file len match

        let FileInfo {
            file: meta_file,
            path: meta_path,
            size: meta_len,
        } = meta;
        _ = read_meta_header(&meta_file, meta_len)?;

        let FileInfo {
            file: data_file,
            path: data_path,
            size: data_len,
        } = data;
        _ = read_data_header(&data_file, data_len)?;

        SPLIT_ACCOUNTS_FILE_STATS
            .num_open
            .fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            meta_path,
            data_path,
            is_dirty: AtomicBool::new(false),
            remove_on_drop: AtomicBool::new(true),
            inner: InnerState::ReadOnly(ReadOnlyState {
                meta_file,
                meta_len,
                data_file,
                data_len,
            }),
        })
    }

    /// Instantiates a new SplitAccountsFile in ready-only mode, or `None` if it already is such.
    pub fn reopen_as_readonly(&self) -> Result<Option<Self>, SplitAccountsFileError> {
        let InnerState::Writable(inner) = &self.inner else {
            // Already in read-only mode; nothing to do.
            return Ok(None);
        };

        // Grab the write lock, there should be no contention since this thread likely
        // just finished writing accounts.  This lets us bypass atomic memory accesses.
        let _write_guard = inner.write_lock.lock().unwrap();

        // we are re-opening the file, so don't remove the file on disk when the old one is dropped
        self.disable_remove_on_drop();

        let meta_file_info = FileInfo {
            file: utils::open_file(&self.meta_path)?,
            path: self.meta_path.clone(),
            size: inner.meta_len.load(Ordering::Relaxed),
        };
        let data_file_info = FileInfo {
            file: utils::open_file(&self.data_path)?,
            path: self.data_path.clone(),
            size: inner.data_len.load(Ordering::Relaxed),
        };
        let mut new = Self::open(meta_file_info, data_file_info)?;

        if self.is_dirty.swap(false, Ordering::Relaxed) {
            // *move* the dirty-ness to `new`
            *new.is_dirty.get_mut() = true;
        }

        Ok(Some(new))
    }

    /// Returns size, in bytes, of meta file.
    pub fn meta_len(&self) -> usize {
        let meta_len = match &self.inner {
            InnerState::ReadOnly(inner) => inner.meta_len,
            InnerState::Writable(inner) => inner.meta_len.load(Ordering::Relaxed),
        };
        meta_len as usize
    }

    /// Returns size, in bytes, of data file.
    pub fn data_len(&self) -> usize {
        let data_len = match &self.inner {
            InnerState::ReadOnly(inner) => inner.data_len,
            InnerState::Writable(inner) => inner.data_len.load(Ordering::Relaxed),
        };
        data_len as usize
    }

    /// Returns total size, in bytes, of both meta and data files.
    pub fn len(&self) -> usize {
        self.meta_len() + self.data_len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn disable_remove_on_drop(&self) {
        self.remove_on_drop.store(false, Ordering::Relaxed);
    }

    /// Flushes contents to disk
    pub fn flush(&self) -> Result<(), SplitAccountsFileError> {
        let was_dirty = self.is_dirty.swap(false, Ordering::Relaxed);
        if !was_dirty {
            // wasn't dirty, so nothing to do here
            return Ok(());
        }

        let (meta_file, data_file) = match &self.inner {
            InnerState::ReadOnly(inner) => (&inner.meta_file, &inner.data_file),
            InnerState::Writable(inner) => (&inner.meta_file, &inner.data_file),
        };
        let meta_result = meta_file
            .sync_all()
            .map_err(SplitAccountsFileError::FlushFile);
        let data_result = data_file
            .sync_all()
            .map_err(SplitAccountsFileError::FlushFile);

        // if the flushes were successful, update the stats,
        // otherwise re-set the is_dirty flag
        let result = meta_result.and(data_result);
        if result.is_ok() {
            SPLIT_ACCOUNTS_FILE_STATS
                .num_dirty
                .fetch_sub(1, Ordering::Relaxed);
        } else {
            self.is_dirty.store(true, Ordering::Relaxed);
        }
        result
    }

    /// Writes `accounts`.
    ///
    /// Returns a vec of the logical offsets for where each account was written.
    pub fn write_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
    ) -> Result<(Vec<LogicalOffset>, u64), SplitAccountsFileError> {
        let InnerState::Writable(inner) = &self.inner else {
            return Err(SplitAccountsFileError::NotWritable);
        };

        let _write_guard = inner.write_lock.lock().unwrap();

        let mut meta_file_offset = FileOffset(inner.meta_len.load(Ordering::Relaxed));
        let mut data_file_offset = FileOffset(inner.data_len.load(Ordering::Relaxed));
        let mut logical_offsets = Vec::with_capacity(accounts.len());

        for i in 0..accounts.len() {
            accounts.account_default_if_zero_lamport(
                i,
                |account| -> Result<(), SplitAccountsFileError> {
                    let data_len = DataLen::try_from(account.data().len())?;
                    let data_ref = if data_len.0 == 0 {
                        // no data, so nothing to write
                        DataRefBorrowed::NoData
                    } else if should_store_account_data_in_meta_file(data_len) {
                        // data is small, so will write inline in the meta file
                        DataRefBorrowed::Inline(account.data())
                    } else {
                        // data is large, so write into the data file now
                        let write_info = write_data_entry(
                            &inner.data_file,
                            data_file_offset,
                            account.pubkey(),
                            account.data(),
                        )?;
                        data_file_offset =
                            FileOffset(write_info.start.0 + write_info.num_bytes_written as u64);
                        DataRefBorrowed::External(ExternalDataOffset(write_info.start))
                    };

                    let write_info = write_meta_entry(
                        &inner.meta_file,
                        meta_file_offset,
                        account.pubkey(),
                        account.owner(),
                        account.lamports(),
                        account.rent_epoch(),
                        account.executable(),
                        data_len,
                        data_ref,
                    )?;
                    meta_file_offset =
                        FileOffset(write_info.start.0 + write_info.num_bytes_written as u64);
                    logical_offsets.push(logical_offset_from_file(write_info.start)?);
                    Ok(())
                },
            )?;
        }

        inner.meta_len.store(meta_file_offset.0, Ordering::Relaxed);
        inner.data_len.store(data_file_offset.0, Ordering::Relaxed);
        let was_dirty = self.is_dirty.swap(true, Ordering::Relaxed);
        if !was_dirty {
            SPLIT_ACCOUNTS_FILE_STATS
                .num_dirty
                .fetch_add(1, Ordering::Relaxed);
        }

        Ok((logical_offsets, meta_file_offset.0 + data_file_offset.0))
    }

    /// Reads account at `offset` and then calls `callback` with it.
    ///
    /// This fn does *not* read the account's data.
    pub fn get_account_without_data<Ret>(
        &self,
        offset: LogicalOffset,
        mut callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Result<Ret, SplitAccountsFileError> {
        let (meta_file, meta_file_len) = match &self.inner {
            InnerState::ReadOnly(inner) => (&inner.meta_file, inner.meta_len),
            InnerState::Writable(inner) => {
                (&inner.meta_file, inner.meta_len.load(Ordering::Relaxed))
            }
        };
        let file_offset = file_offset_from_logical(offset);
        let ret = read_meta_entry(
            meta_file,
            meta_file_len,
            file_offset,
            |meta_entry, _data_ref| callback(stored_account_without_data_from(meta_entry)),
        )?;
        Ok(ret)
    }

    /// Reads account at `offset` and then calls `callback` with it.
    ///
    /// This fn *does* read the account's data.
    pub fn get_account_with_data<Ret>(
        &self,
        offset: LogicalOffset,
        mut callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Result<Ret, SplitAccountsFileError> {
        let (meta_file, meta_file_len) = match &self.inner {
            InnerState::ReadOnly(inner) => (&inner.meta_file, inner.meta_len),
            InnerState::Writable(inner) => {
                (&inner.meta_file, inner.meta_len.load(Ordering::Relaxed))
            }
        };
        let file_offset = file_offset_from_logical(offset);
        read_meta_entry(
            meta_file,
            meta_file_len,
            file_offset,
            |meta_entry, data_ref| -> Result<_, SplitAccountsFileError> {
                match data_ref {
                    DataRefBorrowed::NoData => Ok(callback(stored_account_from(meta_entry, &[]))),
                    DataRefBorrowed::Inline(data) => {
                        Ok(callback(stored_account_from(meta_entry, data)))
                    }
                    DataRefBorrowed::External(external_data_offset) => {
                        let (data_file, data_file_len) = match &self.inner {
                            InnerState::ReadOnly(inner) => (&inner.data_file, inner.data_len),
                            InnerState::Writable(inner) => {
                                (&inner.data_file, inner.data_len.load(Ordering::Relaxed))
                            }
                        };
                        let data = read_data_entry(
                            data_file,
                            data_file_len,
                            external_data_offset.0,
                            meta_entry.address,
                            meta_entry.data_len,
                        )?;
                        Ok(callback(stored_account_from(meta_entry, &data)))
                    }
                }
            },
        )?
    }

    /// Read account at `offset` and return it as an AccountSharedData.
    pub fn get_account_shared_data(
        &self,
        offset: LogicalOffset,
    ) -> Result<AccountSharedData, SplitAccountsFileError> {
        self.get_account_with_data(offset, |stored_account| {
            create_account_shared_data(&stored_account)
        })
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * LogicalOffset: the offset within the file of this account
    /// * StoredAccountInfoWithoutData: the account itself, without account data
    ///
    /// Note that account data is not read/passed to the callback.
    pub fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(LogicalOffset, StoredAccountInfoWithoutData<'local>),
    ) -> Result<(), SplitAccountsFileError> {
        let (meta_file, meta_file_len) = match &self.inner {
            InnerState::ReadOnly(inner) => (&inner.meta_file, inner.meta_len),
            InnerState::Writable(inner) => {
                (&inner.meta_file, inner.meta_len.load(Ordering::Relaxed))
            }
        };
        let mut meta_reader =
            BufferedReader::<META_SCAN_BUFFER_SIZE>::new().with_file(meta_file, meta_file_len);
        meta_reader.consume_or_skip(META_HEADER_SIZE);

        while meta_reader.get_file_offset() < meta_file_len {
            let file_offset = FileOffset(meta_reader.get_file_offset());
            let logical_offset = logical_offset_from_file(file_offset)?;
            let buffer_bytes = meta_reader.fill_buf_required(META_ENTRY_FIXED_SIZE)?;
            let meta_entry = parse_meta_entry_fixed(buffer_bytes)?;
            let meta_entry_stored_size = calculate_meta_entry_stored_size(meta_entry.data_len);
            validate_meta_entry_stored_size(meta_file_len, file_offset, meta_entry_stored_size)?;
            callback(logical_offset, stored_account_without_data_from(meta_entry));
            // Move the reader to the next account.
            // The next meta entry's offset must be properly aligned.
            meta_reader.consume_or_skip(
                meta_entry_stored_size.next_multiple_of(META_ENTRY_OFFSET_ALIGNMENT),
            );
        }
        Ok(())
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * LogicalOffset: the offset within the file of this account
    /// * StoredAccountInfo: the account itself, with account data
    ///
    /// Prefer scan_accounts_without_data() when account data is not needed,
    /// as it can read less and be faster.
    pub fn scan_accounts_with_data<'a>(
        &'a self,
        data_reader: &mut impl RequiredLenBufFileRead<'a>,
        mut callback: impl for<'local> FnMut(LogicalOffset, StoredAccountInfo<'local>),
    ) -> Result<(), SplitAccountsFileError> {
        let (meta_file, meta_file_len, data_file, data_file_len) = match &self.inner {
            InnerState::ReadOnly(inner) => (
                &inner.meta_file,
                inner.meta_len,
                &inner.data_file,
                inner.data_len,
            ),
            InnerState::Writable(inner) => (
                &inner.meta_file,
                inner.meta_len.load(Ordering::Relaxed),
                &inner.data_file,
                inner.data_len.load(Ordering::Relaxed),
            ),
        };
        let mut meta_reader =
            BufferedReader::<META_SCAN_BUFFER_SIZE>::new().with_file(meta_file, meta_file_len);
        meta_reader.consume_or_skip(META_HEADER_SIZE);
        data_reader.set_file(data_file, data_file_len)?;

        let mut required_meta_read_size = META_ENTRY_FIXED_SIZE;
        while meta_reader.get_file_offset() < meta_file_len {
            let file_offset = FileOffset(meta_reader.get_file_offset());
            let logical_offset = logical_offset_from_file(file_offset)?;
            let buffer_bytes = meta_reader.fill_buf_required(required_meta_read_size)?;
            let meta_entry = parse_meta_entry_fixed(buffer_bytes)?;
            let meta_entry_stored_size = calculate_meta_entry_stored_size(meta_entry.data_len);
            validate_meta_entry_stored_size(meta_file_len, file_offset, meta_entry_stored_size)?;
            if buffer_bytes.len() < meta_entry_stored_size {
                // The buffer didn't already read the data_ref for this meta entry,
                // so increase the required size and try again.
                required_meta_read_size = meta_entry_stored_size;
                continue;
            }
            // Reset the required size back to the default.
            required_meta_read_size = META_ENTRY_FIXED_SIZE;

            match parse_meta_entry_data_ref(buffer_bytes, &meta_entry)? {
                DataRefBorrowed::NoData => {
                    callback(logical_offset, stored_account_from(meta_entry, &[]));
                }
                DataRefBorrowed::Inline(data) => {
                    callback(logical_offset, stored_account_from(meta_entry, data));
                }
                DataRefBorrowed::External(external_data_offset) => {
                    validate_data_entry_offset(
                        data_file_len,
                        external_data_offset.0,
                        meta_entry.data_len,
                    )?;
                    advance_reader_position_to(data_reader, external_data_offset.0)?;
                    let data_entry_stored_size =
                        calculate_data_entry_stored_size(meta_entry.data_len.0 as usize);
                    let data_bytes = data_reader.fill_buf_required(data_entry_stored_size)?;
                    let account_data =
                        parse_data_entry(data_bytes, meta_entry.address, meta_entry.data_len)?;
                    callback(
                        logical_offset,
                        stored_account_from(meta_entry, account_data),
                    );
                }
            }

            // Move the reader to the next account.
            // The next meta entry's offset must be properly aligned.
            meta_reader.consume_or_skip(
                meta_entry_stored_size.next_multiple_of(META_ENTRY_OFFSET_ALIGNMENT),
            );
        }
        Ok(())
    }

    /// Returns a vec of account data sizes for each account in `sorted_offsets`.
    pub fn get_account_data_lens(
        &self,
        sorted_offsets: &[LogicalOffset],
    ) -> Result<Vec<usize>, SplitAccountsFileError> {
        let mut data_lens = Vec::with_capacity(sorted_offsets.len());
        for offset in sorted_offsets {
            let data_len =
                self.get_account_without_data(*offset, |stored_account| stored_account.data_len)?;
            data_lens.push(data_len);
        }
        Ok(data_lens)
    }

    /// Returns the number of bytes to store an account with data of size `data_len`.
    pub fn calculate_stored_size(data_len: usize) -> usize {
        let data_len = DataLen::try_from(data_len).unwrap();
        let meta_size = calculate_meta_entry_stored_size(data_len);
        let data_size = if should_store_account_data_in_meta_file(data_len) {
            0
        } else {
            calculate_data_entry_stored_size(data_len.0 as usize)
        };
        meta_size + data_size
    }

    /// Returns the number of bytes required to store a closed account.
    pub fn dead_bytes_due_to_zero_lamport_accounts(&self, count: usize) -> usize {
        Self::calculate_stored_size(0) * count
    }
}

/// Helper fn to return a StoredAccountInfo from a meta entry.
fn stored_account_from<'a>(meta_entry: MetaEntryRef<'a>, data: &'a [u8]) -> StoredAccountInfo<'a> {
    StoredAccountInfo {
        pubkey: meta_entry.address,
        owner: meta_entry.owner,
        lamports: meta_entry.lamports,
        rent_epoch: meta_entry.rent_epoch,
        executable: meta_entry.is_executable,
        data,
    }
}

/// Helper fn to return a StoredAccountInfoWithoutData from a meta entry.
fn stored_account_without_data_from<'a>(
    meta_entry: MetaEntryRef<'a>,
) -> StoredAccountInfoWithoutData<'a> {
    StoredAccountInfoWithoutData {
        pubkey: meta_entry.address,
        owner: meta_entry.owner,
        lamports: meta_entry.lamports,
        rent_epoch: meta_entry.rent_epoch,
        executable: meta_entry.is_executable,
        data_len: meta_entry.data_len.0 as usize,
    }
}

/// Validates the `offset` for a given meta entry's `stored_size`.
///
/// Used when scanning, and checks:
/// * if the offset + stored size overflows
/// * if the offset + stored size is greater than `file_len`
fn validate_meta_entry_stored_size(
    file_len: FileSize,
    offset: FileOffset,
    stored_size: usize,
) -> Result<(), SplitAccountsFileError> {
    if offset
        .0
        .checked_add(stored_size as FileSize)
        .is_none_or(|end| end > file_len)
    {
        return Err(error::ReadMetaEntryError::OffsetOverrun(offset).into());
    }
    Ok(())
}

/// Advances `reader` to `offset`.
///
/// Note, this advances *to* offset, not *by* offset.
/// Also, offset must be greater-than-or-equal to the reader's current position;
/// it cannot move backwards.  An error is returned otherwise.
fn advance_reader_position_to<'a>(
    reader: &mut impl agave_fs::buffered_reader::FileBufRead<'a>,
    offset: FileOffset,
) -> Result<(), SplitAccountsFileError> {
    let bytes_to_skip = offset
        .0
        .checked_sub(reader.get_file_offset())
        .ok_or(SplitAccountsFileError::InvalidFileOffset(offset))?;
    reader.consume_or_skip(bytes_to_skip as usize);
    Ok(())
}

/// The inner state of a SplitAccountsFile, used to distinguish writable from read-only state.
#[derive(Debug)]
enum InnerState {
    ReadOnly(ReadOnlyState),
    Writable(WritableState),
}

#[derive(Debug)]
struct ReadOnlyState {
    meta_file: File,
    meta_len: u64,
    data_file: File,
    data_len: u64,
}

#[derive(Debug)]
struct WritableState {
    write_lock: Mutex<()>,
    meta_file: File,
    meta_len: AtomicU64,
    data_file: File,
    data_len: AtomicU64,
}

#[derive(Debug)]
pub struct SplitAccountsFileStats {
    pub num_open: AtomicU64,
    pub num_dirty: AtomicU64,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        error::{ReadDataEntryError, ReadMetaEntryError},
        meta::META_ENTRY_INLINE_DATA_MAX_SIZE,
        solana_account::accounts_equal,
        solana_pubkey::Pubkey,
        std::assert_matches,
        tempfile::TempDir,
        test_case::test_case,
    };

    /// Ensure creating a new SplitAccountsFile works.
    #[test]
    fn test_new() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        _ = SplitAccountsFile::new(&base_path).unwrap();
    }

    /// Ensure opening an existing SplitAccountsFile works.
    #[test]
    fn test_open() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let split = SplitAccountsFile::new(&base_path).unwrap();

        let meta = FileInfo::new_from_path(&split.meta_path).unwrap();
        let data = FileInfo::new_from_path(&split.data_path).unwrap();
        _ = SplitAccountsFile::open(meta, data).unwrap();
    }

    /// Ensure we can reopen a SplitAccountsFile as read-only.
    #[test]
    fn test_reopen_as_readonly() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let split1 = SplitAccountsFile::new(&base_path).unwrap();

        // reopen should succeed
        let split2 = split1.reopen_as_readonly().unwrap().unwrap();

        // since split2 is already read-only, reopening again will return None
        let split3 = split2.reopen_as_readonly().unwrap();
        assert!(split3.is_none());
    }

    /// Ensure writing and reading accounts works.
    #[test]
    fn test_write_and_read_accounts() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let split_writable = SplitAccountsFile::new(&base_path).unwrap();

        let slot = 13;
        let owner = Pubkey::new_unique();
        let no_data = 0;
        let inline_data_min = no_data + 1;
        let inline_data_max = META_ENTRY_INLINE_DATA_MAX_SIZE;
        let external_data_min = inline_data_max + 1;
        let external_data_max = DataLen::MAX as usize;
        let accounts = [
            (
                Pubkey::new_unique(),
                AccountSharedData::new(101, inline_data_min, &owner),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(102, no_data, &owner),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(103, external_data_max, &owner),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(104, inline_data_max, &owner),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(105, external_data_min, &owner),
            ),
        ];

        let (written_logical_offsets, _written_size) = split_writable
            .write_accounts(&(slot, accounts.as_slice()))
            .unwrap();

        // test both readonly and writable files
        let split_readonly = split_writable.reopen_as_readonly().unwrap().unwrap();
        for split in [&split_writable, &split_readonly] {
            for i in 0..accounts.len() {
                let (address, account) = &accounts[i];
                let logical_offset = written_logical_offsets[i];

                let loaded_account = split.get_account_shared_data(logical_offset).unwrap();
                assert_eq!(&loaded_account, account);

                split
                    .get_account_with_data(logical_offset, |stored_account| {
                        assert_eq!(stored_account.pubkey, address);
                        assert!(accounts_equal(&stored_account, &account));
                    })
                    .unwrap();

                split
                    .get_account_without_data(logical_offset, |stored_account| {
                        assert_eq!(stored_account.pubkey, address);
                        assert_eq!(stored_account.owner, account.owner());
                        assert_eq!(stored_account.lamports, account.lamports());
                        assert_eq!(stored_account.rent_epoch, account.rent_epoch());
                        assert_eq!(stored_account.executable, account.executable());
                        assert_eq!(stored_account.data_len, account.data().len());
                    })
                    .unwrap();
            }
        }
    }

    /// Ensure the scan_accounts() fns work.
    #[test]
    fn test_scan_accounts() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let split_writable = SplitAccountsFile::new(&base_path).unwrap();

        let slot = 23;
        let owner = Pubkey::new_unique();
        let no_data = 0;
        let inline_data = 165; // 165 bytes is the common size for a token account
        let small_external_data = META_ENTRY_INLINE_DATA_MAX_SIZE + 1;
        // Include accounts larger than MIN_CAPACITY (128 KB) of the reader
        // to exercise the data reader's overflow growth.
        let large_external_data = 2_000_000;
        let mut accounts = Vec::new();
        for i in 0..5 {
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, inline_data + i, &owner),
            ));
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, no_data, &owner),
            ));
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, small_external_data + i, &owner),
            ));
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, large_external_data + i, &owner),
            ));
        }
        // Include enough accounts to ensure the meta file size is larger than
        // the META_SCAN_BUFFER_SIZE to exercise the meta reader's retry logic.
        for _ in 0..META_SCAN_BUFFER_SIZE.div_ceil(META_ENTRY_INLINE_DATA_MAX_SIZE) {
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, META_ENTRY_INLINE_DATA_MAX_SIZE, &owner),
            ));
        }

        let (written_logical_offsets, _written_size) = split_writable
            .write_accounts(&(slot, accounts.as_slice()))
            .unwrap();

        assert!(split_writable.meta_len() > META_SCAN_BUFFER_SIZE);

        // test both readonly and writable files
        let split_readonly = split_writable.reopen_as_readonly().unwrap().unwrap();
        for split in [&split_writable, &split_readonly] {
            let mut data_reader = new_scan_accounts_reader();
            let mut i = 0;
            split
                .scan_accounts_with_data(&mut data_reader, |offset, stored_account| {
                    assert_eq!(offset, written_logical_offsets[i]);
                    assert_eq!(stored_account.pubkey, &accounts[i].0);
                    assert!(accounts_equal(&stored_account, &accounts[i].1));
                    i += 1;
                })
                .unwrap();
            // ensure the scan visited all the accounts and didn't silently terminate
            assert_eq!(i, accounts.len());

            let mut i = 0;
            split
                .scan_accounts_without_data(|offset, stored_account| {
                    assert_eq!(offset, written_logical_offsets[i]);
                    assert_eq!(stored_account.pubkey, &accounts[i].0);
                    let account = &accounts[i].1;
                    assert_eq!(stored_account.owner, account.owner());
                    assert_eq!(stored_account.lamports, account.lamports());
                    assert_eq!(stored_account.rent_epoch, account.rent_epoch());
                    assert_eq!(stored_account.executable, account.executable());
                    assert_eq!(stored_account.data_len, account.data().len());
                    i += 1;
                })
                .unwrap();
            // ensure the scan visited all the accounts and didn't silently terminate
            assert_eq!(i, accounts.len());
        }
    }

    /// Test when the fixed portion of the meta entry is truncated.
    #[test]
    fn test_get_and_scan_truncated_meta_entry_no_data() {
        let temp_dir = TempDir::new().unwrap();
        let split = SplitAccountsFile::new(temp_dir.path().join("base")).unwrap();
        let address = Pubkey::new_unique();
        let account = AccountSharedData::new(123, 0, &Pubkey::default());
        let (offsets, _) = split
            .write_accounts(&(0, [(address, account)].as_slice()))
            .unwrap();

        // truncate the meta file so account loads fail
        match &split.inner {
            InnerState::Writable(inner) => inner.meta_len.fetch_sub(1, Ordering::Relaxed),
            _ => unreachable!(),
        };

        // test case: get_account_without_data()
        {
            let err = split
                .get_account_without_data(offsets[0], |_| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::OffsetOverrun(_))
            );
        }

        // test case: get_account_with_data()
        {
            let err = split.get_account_with_data(offsets[0], |_| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::OffsetOverrun(_))
            );
        }

        // test case: scan_accounts_without_data()
        {
            let err = split.scan_accounts_without_data(|_, _| {}).unwrap_err();
            assert_matches!(err, SplitAccountsFileError::Io(_));
        }

        // test case: can_accounts_with_data()
        {
            let mut data_reader = new_scan_accounts_reader();
            let err = split
                .scan_accounts_with_data(&mut data_reader, |_, _| {})
                .unwrap_err();
            assert_matches!(err, SplitAccountsFileError::Io(_));
        }
    }

    /// Test when a meta entry with inline data is truncated.
    #[test]
    fn test_get_and_scan_truncated_meta_entry_inline_data() {
        let temp_dir = TempDir::new().unwrap();
        let split = SplitAccountsFile::new(temp_dir.path().join("base")).unwrap();
        let address = Pubkey::new_unique();
        let account = AccountSharedData::new(123, 165, &Pubkey::default());
        let (offsets, _) = split
            .write_accounts(&(0, [(address, account)].as_slice()))
            .unwrap();

        // truncate the meta file so account loads fail
        match &split.inner {
            InnerState::Writable(inner) => inner.meta_len.fetch_sub(1, Ordering::Relaxed),
            _ => unreachable!(),
        };

        // test case: get_account_without_data()
        {
            let err = split
                .get_account_without_data(offsets[0], |_| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::ShortRead { .. })
            );
        }

        // test case: get_account_with_data()
        {
            let err = split.get_account_with_data(offsets[0], |_| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::ShortRead { .. })
            );
        }

        // test case: scan_accounts_without_data()
        {
            let err = split.scan_accounts_without_data(|_, _| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::OffsetOverrun(_))
            );
        }

        // test case: scan_accounts_with_data()
        {
            let mut data_reader = new_scan_accounts_reader();
            let err = split
                .scan_accounts_with_data(&mut data_reader, |_, _| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::OffsetOverrun(_))
            );
        }
    }

    /// Test when a meta entry with external data is truncated.
    #[test]
    fn test_get_and_scan_truncated_meta_entry_external_data() {
        let temp_dir = TempDir::new().unwrap();
        let split = SplitAccountsFile::new(temp_dir.path().join("base")).unwrap();
        let address = Pubkey::new_unique();
        // account data must be large enough to ensure it is written into external data
        let data_len = META_ENTRY_INLINE_DATA_MAX_SIZE + 1;
        let account = AccountSharedData::new(123, data_len, &Pubkey::default());
        let (offsets, _) = split
            .write_accounts(&(0, [(address, account)].as_slice()))
            .unwrap();

        // truncate the meta file so account loads fail
        match &split.inner {
            InnerState::Writable(inner) => inner.meta_len.fetch_sub(1, Ordering::Relaxed),
            _ => unreachable!(),
        };

        // test case: get_account_without_data()
        {
            let err = split
                .get_account_without_data(offsets[0], |_| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::ShortRead { .. })
            );
        }

        // test case: get_account_with_data()
        {
            let err = split.get_account_with_data(offsets[0], |_| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::ShortRead { .. })
            );
        }

        // test case: scan_accounts_without_data()
        {
            let err = split.scan_accounts_without_data(|_, _| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::OffsetOverrun(_))
            );
        }

        // test case: scan_accounts_with_data()
        {
            let mut data_reader = new_scan_accounts_reader();
            let err = split
                .scan_accounts_with_data(&mut data_reader, |_, _| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadMetaEntry(ReadMetaEntryError::OffsetOverrun(_))
            );
        }
    }

    /// Test when a data entry is truncated.
    #[test]
    fn test_get_and_scan_truncated_data_entry() {
        let temp_dir = TempDir::new().unwrap();
        let split = SplitAccountsFile::new(temp_dir.path().join("base")).unwrap();
        let address = Pubkey::new_unique();
        // account data must be large enough to ensure it is written into external data
        let data_len = META_ENTRY_INLINE_DATA_MAX_SIZE + 1;
        let account = AccountSharedData::new(123, data_len, &Pubkey::default());
        let (offsets, _) = split
            .write_accounts(&(0, [(address, account)].as_slice()))
            .unwrap();

        // truncate the data file so account loads fail
        match &split.inner {
            InnerState::Writable(inner) => inner.data_len.fetch_sub(1, Ordering::Relaxed),
            _ => unreachable!(),
        };

        // test case: get_account_without_data()
        {
            // does not fail because the data entry is not read
            split.get_account_without_data(offsets[0], |_| {}).unwrap();
        }

        // test case: get_account_with_data()
        {
            let err = split.get_account_with_data(offsets[0], |_| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadDataEntry(ReadDataEntryError::OffsetOverrun(_))
            );
        }

        // test case: scan_accounts_without_data()
        {
            // does not fail because the data entry is not read
            split.scan_accounts_without_data(|_, _| {}).unwrap();
        }

        // test case: scan_accounts_with_data()
        {
            let mut data_reader = new_scan_accounts_reader();
            let err = split
                .scan_accounts_with_data(&mut data_reader, |_, _| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadDataEntry(ReadDataEntryError::OffsetOverrun(_))
            );
        }
    }

    /// Test when a data entry's address does not match.
    #[test]
    fn test_get_and_scan_external_data_address_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let split = SplitAccountsFile::new(temp_dir.path().join("base")).unwrap();
        let expected_address = Pubkey::new_unique();
        // account data must be large enough to ensure it is written into external data
        let data_len = META_ENTRY_INLINE_DATA_MAX_SIZE + 1;
        let account = AccountSharedData::new(123, data_len, &Pubkey::default());
        let (offsets, _) = split
            .write_accounts(&(0, [(&expected_address, &account)].as_slice()))
            .unwrap();

        let data_file = match &split.inner {
            InnerState::Writable(inner) => &inner.data_file,
            _ => unreachable!(),
        };

        // modify the data entry to have a different address
        let data_offset =
            data::DATA_HEADER_SIZE.next_multiple_of(data::DATA_ENTRY_OFFSET_ALIGNMENT) as u64;
        let wrong_address = Pubkey::new_unique();
        write_data_entry(
            data_file,
            FileOffset(data_offset),
            &wrong_address,
            vec![0; data_len].as_slice(),
        )
        .unwrap();

        // test case: get_account_without_data()
        {
            // does not fail because the data entry is not read
            split.get_account_without_data(offsets[0], |_| {}).unwrap();
        }

        // test case: get_account_with_data()
        {
            let err = split.get_account_with_data(offsets[0], |_| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadDataEntry(ReadDataEntryError::AddressMismatch { .. }),
            );
        }

        // test case: scan_accounts_without_data()
        {
            // does not fail because the data entry is not read
            split.scan_accounts_without_data(|_, _| {}).unwrap();
        }

        // test case: scan_accounts_with_data()
        {
            let mut data_reader = new_scan_accounts_reader();
            let err = split
                .scan_accounts_with_data(&mut data_reader, |_, _| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadDataEntry(ReadDataEntryError::AddressMismatch { .. }),
            );
        }
    }

    /// Test when a data entry's account data size does not match.
    #[test]
    fn test_get_and_scan_external_data_len_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let split = SplitAccountsFile::new(temp_dir.path().join("base")).unwrap();
        let address = Pubkey::new_unique();
        // account data must be large enough to ensure it is written into external data
        let expected_data_len = META_ENTRY_INLINE_DATA_MAX_SIZE + 1;
        let account = AccountSharedData::new(123, expected_data_len, &Pubkey::default());
        let (offsets, _) = split
            .write_accounts(&(0, [(address, account)].as_slice()))
            .unwrap();

        let data_file = match &split.inner {
            InnerState::Writable(inner) => &inner.data_file,
            _ => unreachable!(),
        };

        // modify the data entry to have a different data len
        let data_offset =
            data::DATA_HEADER_SIZE.next_multiple_of(data::DATA_ENTRY_OFFSET_ALIGNMENT) as u64;
        let wrong_data_len = expected_data_len - 1;
        write_data_entry(
            data_file,
            FileOffset(data_offset),
            &address,
            vec![0; wrong_data_len].as_slice(),
        )
        .unwrap();

        // test case: get_account_without_data()
        {
            // does not fail because the data entry is not read
            split.get_account_without_data(offsets[0], |_| {}).unwrap();
        }

        // test case: get_account_with_data()
        {
            let err = split.get_account_with_data(offsets[0], |_| {}).unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadDataEntry(ReadDataEntryError::DataLenMismatch { .. }),
            );
        }

        // test case: scan_accounts_without_data()
        {
            // does not fail because the data entry is not read
            split.scan_accounts_without_data(|_, _| {}).unwrap();
        }

        // test case: scan_accounts_with_data()
        {
            let mut data_reader = new_scan_accounts_reader();
            let err = split
                .scan_accounts_with_data(&mut data_reader, |_, _| {})
                .unwrap_err();
            assert_matches!(
                err,
                SplitAccountsFileError::ReadDataEntry(ReadDataEntryError::DataLenMismatch { .. }),
            );
        }
    }

    /// Ensure get_account_data_lens() works.
    #[test]
    fn test_get_account_data_lens() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let split = SplitAccountsFile::new(&base_path).unwrap();

        let slot = 33;
        let owner = Pubkey::new_unique();
        let no_data = 0;
        let inline_data = 42;
        let external_data = meta::META_ENTRY_MAX_SIZE + 1;
        let mut accounts = Vec::new();
        for i in 0..2 {
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, inline_data + i, &owner),
            ));
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, no_data, &owner),
            ));
            accounts.push((
                Pubkey::new_unique(),
                AccountSharedData::new(123, external_data + i, &owner),
            ));
        }

        let (written_offsets, _written_size) =
            split.write_accounts(&(slot, accounts.as_slice())).unwrap();

        let offsets = [written_offsets[2], written_offsets[3], written_offsets[4]];
        let data_lens = split.get_account_data_lens(offsets.as_slice()).unwrap();
        assert_eq!(data_lens.len(), offsets.len());
        assert_eq!(data_lens[0], accounts[2].1.data().len());
        assert_eq!(data_lens[1], accounts[3].1.data().len());
        assert_eq!(data_lens[2], accounts[4].1.data().len());
    }

    // Ensure `is_dirty` is tracked properly.
    //
    // In particular:
    // * `reopen_as_readonly()` moves `is_dirty`
    // * `flush()` clears `is_dirty`
    #[test_case(false)]
    #[test_case(true)]
    fn test_is_dirty(begins_dirty: bool) {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().join("base");
        let mut split1 = SplitAccountsFile::new(&base_path).unwrap();

        // ensure the split file begins not dirty
        assert!(!*split1.is_dirty.get_mut());

        // set initial state
        *split1.is_dirty.get_mut() = begins_dirty;

        // ensure reopen() moves `is_dirty`
        let mut split2 = split1.reopen_as_readonly().unwrap().unwrap();
        assert!(!*split1.is_dirty.get_mut());
        assert_eq!(*split2.is_dirty.get_mut(), begins_dirty);

        // ensure we can flush the new split file
        assert!(split2.flush().is_ok());
        // and now should not be dirty
        assert!(!*split2.is_dirty.get_mut());

        // ensure we can flush the old split file too
        assert!(split1.flush().is_ok());
        // and should not be dirty still
        assert!(!*split1.is_dirty.get_mut());
    }
}
