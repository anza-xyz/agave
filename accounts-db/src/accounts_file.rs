use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_db::AccountsFileId,
        append_vec::{AppendVec, AppendVecError, MAXIMUM_APPEND_VEC_FILE_SIZE},
        split_file::{self, SplitFile, SplitFileError},
        storable_accounts::StorableAccounts,
    },
    agave_fs::{
        FileInfo, FileSize,
        buffered_reader::{
            BufReaderWithOverflow, BufferedReader, FileBufRead, RequiredLenBufFileRead,
        },
        file_io::open_for_reading,
    },
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    std::{
        fs::{self, File},
        io,
        iter::ExactSizeIterator,
        mem,
        path::{Path, PathBuf},
        slice,
    },
    thiserror::Error,
};

// Data placement should be aligned at the next boundary. Without alignment accessing the memory may
// crash on some architectures.
pub const ALIGN_BOUNDARY_OFFSET: usize = mem::size_of::<u64>();
#[macro_export]
macro_rules! u64_align {
    ($addr: expr) => {
        ($addr + ($crate::accounts_file::ALIGN_BOUNDARY_OFFSET - 1))
            & !($crate::accounts_file::ALIGN_BOUNDARY_OFFSET - 1)
    };
}

pub type Result<T> = std::result::Result<T, AccountsFileError>;

/// An enum for AccountsFile related errors.
#[derive(Error, Debug)]
pub enum AccountsFileError {
    #[error("AppendVecError: {0}")]
    AppendVecError(#[from] AppendVecError),

    #[error("SplitFileError: {0}")]
    SplitFileError(#[from] SplitFileError),

    // generic io::Error is last so other variants are selected first
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
/// An enum for accessing an accounts file which can be implemented
/// under different formats.
pub enum AccountsFile {
    AppendVec(AppendVec),
    Split(SplitFile),
}

impl AccountsFile {
    /// Creates a new AccountsFile by opening an existing AppendVec from `file_info`.
    pub fn open_append_vec(file_info: FileInfo) -> Result<Self> {
        let av = AppendVec::new_for_startup(file_info)?;
        Ok(Self::AppendVec(av))
    }

    /// Creates a new AccountsFile by opening an existing SplitFile from `meta` and `data`.
    pub fn open_split(meta: FileInfo, data: Option<FileInfo>) -> Result<Self> {
        Ok(Self::Split(SplitFile::open(meta, data)?))
    }

    /// Creates a new AccountsFile by opening an existing AppendVec from `file_info`.
    ///
    /// If the file is too large to fit into an AppendVec instance, copy
    /// the accounts into a new SplitFile and remove the old file.
    ///
    /// This fn is only intended to be called at startup, since
    /// it may perform large amounts of disk io.
    pub fn open_append_vec_for_startup(file_info: FileInfo) -> Result<Self> {
        if file_info.size <= MAXIMUM_APPEND_VEC_FILE_SIZE {
            Self::open_append_vec(file_info)
        } else {
            log::info!(
                "Converting AppendVec to Split: '{}'...",
                file_info.path.display(),
            );
            let accounts_file = Self::new_split_from_append_vec(&file_info)?;
            fs::remove_file(&file_info.path).map_err(|err| {
                io::Error::other(format!(
                    "failed to remove AppendVec '{}' after converting to Split: {err}",
                    file_info.path.display(),
                ))
            })?;
            Ok(accounts_file)
        }
    }

    /// Creates a new SplitFile from `append_vec_file_info`.
    ///
    /// Reads accounts from the append vec and writes them into a new split file.
    /// Returns an AccountsFile instance that is already read-only.
    fn new_split_from_append_vec(append_vec_file_info: &FileInfo) -> Result<Self> {
        let split = SplitFile::new(&append_vec_file_info.path)?;
        let mut reader = new_scan_accounts_reader();
        reader.set_file(&append_vec_file_info.file, append_vec_file_info.size)?;
        let mut first_error = None;
        AppendVec::scan_reader(&mut reader, |account| {
            let result = split.write_accounts(&(0, slice::from_ref(&&account), 0));
            first_error = first_error.take().or(result.err());
        })?;
        if let Some(first_error) = first_error {
            return Err(first_error.into());
        }

        // SAFETY: `split` is *not* read-only, so must return Some.
        let split = split.reopen_as_readonly()?.unwrap();
        Ok(Self::Split(split))
    }

    /// if storage is not readonly, reopen another instance that is read only
    pub(crate) fn reopen_as_readonly(&self) -> Result<Option<Self>> {
        Ok(match self {
            Self::AppendVec(av) => av.reopen_as_readonly_file_io()?.map(Self::AppendVec),
            Self::Split(split) => split.reopen_as_readonly()?.map(Self::Split),
        })
    }

    /// Detach the on-disk file from this storage's lifetime; see
    /// [`AppendVec::disable_remove_on_drop`].
    pub fn disable_remove_on_drop(&self) {
        match self {
            Self::AppendVec(av) => av.disable_remove_on_drop(),
            Self::Split(split) => split.disable_remove_on_drop(),
        }
    }

    /// Flushes contents to disk
    pub fn flush(&self) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.flush()?,
            Self::Split(split) => split.flush()?,
        }
        Ok(())
    }

    /// Returns the number of bytes, *not accounts*, used in the AccountsFile
    pub fn len(&self) -> usize {
        match self {
            Self::AppendVec(av) => av.len(),
            Self::Split(split) => split.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::AppendVec(av) => av.is_empty(),
            Self::Split(split) => split.is_empty(),
        }
    }

    pub fn file_name(slot: Slot, id: AccountsFileId) -> String {
        format!("{slot}.{id}")
    }

    /// Calls `callback` with the stored account at `offset`.
    ///
    /// Returns `None` if there is no account at `offset`, otherwise returns the result of
    /// `callback` in `Some`.
    ///
    /// This fn does *not* load the account's data, just the data length.  If the data is needed,
    /// use `get_stored_account_callback()` instead.  However, prefer this fn when possible.
    pub fn get_stored_account_without_data_callback<Ret>(
        &self,
        offset: Offset,
        callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Result<Ret> {
        Ok(match self {
            Self::AppendVec(av) => av
                .get_stored_account_without_data_callback(offset, callback)
                .ok_or_else(|| io::Error::other("AppendVec did not load an account"))?,
            Self::Split(split) => {
                split.get_account_without_data(split_file::LogicalOffset(offset), callback)?
            }
        })
    }

    /// Calls `callback` with the stored account at `offset`.
    ///
    /// Returns `None` if there is no account at `offset`, otherwise returns the result of
    /// `callback` in `Some`.
    ///
    /// This fn *does* load the account's data.  If the data is not needed,
    /// use `get_stored_account_without_data_callback()` instead.
    pub fn get_stored_account_callback<Ret>(
        &self,
        offset: Offset,
        callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Result<Ret> {
        Ok(match self {
            Self::AppendVec(av) => av
                .get_stored_account_callback(offset, callback)
                .ok_or_else(|| io::Error::other("AppendVec did not load an account"))?,
            Self::Split(split) => {
                split.get_account_with_data(split_file::LogicalOffset(offset), callback)?
            }
        })
    }

    /// return an `AccountSharedData` for an account at `offset`, if any.  Otherwise return None.
    pub(crate) fn get_account_shared_data(&self, offset: Offset) -> Result<AccountSharedData> {
        Ok(match self {
            Self::AppendVec(av) => av
                .get_account_shared_data(offset)
                .ok_or_else(|| io::Error::other("AppendVec did not load an account"))?,
            Self::Split(split) => {
                split.get_account_shared_data(split_file::LogicalOffset(offset))?
            }
        })
    }

    /// Return the path of the underlying account file.
    pub fn path(&self) -> &Path {
        match self {
            Self::AppendVec(av) => av.path(),
            Self::Split(split) => {
                // only used in error messages; may want to use base path instead later
                split.meta_path()
            }
        }
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfoWithoutData: the account itself, without account data
    ///
    /// Note that account data is not read/passed to the callback.
    pub fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_accounts_without_data(callback)?,
            Self::Split(split) => split.scan_accounts_without_data(|logical_offset, account| {
                let split_file::LogicalOffset(offset) = logical_offset;
                callback(offset, account)
            })?,
        }
        Ok(())
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfo: the account itself, with account data
    ///
    /// Prefer scan_accounts_without_data() when account data is not needed,
    /// as it can potentially read less and be faster.
    #[cfg(test)]
    pub(crate) fn scan_accounts<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> Result<()> {
        self.set_file_on_reader(reader)?;
        self.scan_accounts_with(reader, callback)
    }

    /// Activates this instance's account data file on `reader`.
    pub(crate) fn set_file_on_reader<'a>(
        &'a self,
        reader: &mut impl FileBufRead<'a>,
    ) -> io::Result<()> {
        if let Some((file, read_limit)) = self.account_data_file() {
            reader.set_file(file, read_limit)
        } else {
            Ok(())
        }
    }

    /// Scans the file already activated on the reader, preserving its read-ahead and I/O mode.
    pub(crate) fn scan_accounts_with<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_accounts_with(reader, callback)?,
            Self::Split(split) => {
                split.scan_accounts_with_data(reader, |logical_offset, account| {
                    let split_file::LogicalOffset(offset) = logical_offset;
                    callback(offset, account)
                })?
            }
        }
        Ok(())
    }

    /// Returns the number of bytes requried to store an account with the pass in data_len.
    ///
    /// Note, this is the size to store accounts into AppendVec format.
    pub(crate) fn calculate_stored_size(&self, data_len: usize) -> usize {
        AppendVec::calculate_stored_size(data_len)
    }

    /// Returns the account data size for each account in `offsets`.
    pub(crate) fn get_account_data_lens(
        &self,
        offsets: impl IntoIterator<Item = Offset, IntoIter: ExactSizeIterator>,
    ) -> Vec<usize> {
        match self {
            Self::AppendVec(av) => av.get_account_data_lens(offsets),
            Self::Split(split) => {
                let logical_offsets = offsets.into_iter().map(split_file::LogicalOffset);
                split
                    .get_account_data_lens(logical_offsets)
                    .expect("split file offsets must be valid")
            }
        }
    }

    /// iterate over all pubkeys
    pub fn scan_pubkeys(&self, mut callback: impl FnMut(&Pubkey)) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_pubkeys(callback)?,
            Self::Split(split) => {
                split.scan_accounts_without_data(|_offset, account| callback(account.pubkey))?
            }
        }
        Ok(())
    }

    /// Writes `accounts` to the file.
    ///
    /// Returns the starting offset of each written account.
    pub fn write_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
    ) -> Result<StoredAccountsInfo> {
        match self {
            Self::AppendVec(av) => Ok(av
                .append_accounts(accounts)
                .ok_or_else(|| io::Error::other("AppendVec did not write any accounts"))?),
            Self::Split(split) => {
                let (logical_offsets, stored_size) = split.write_accounts(accounts)?;
                let offsets = logical_offsets
                    .into_iter()
                    .map(|logical_offset| {
                        let split_file::LogicalOffset(offset) = logical_offset;
                        offset
                    })
                    .collect();
                Ok(StoredAccountsInfo {
                    offsets,
                    size: stored_size as usize,
                })
            }
        }
    }

    /// Returns the file holding the account data and its length.
    fn account_data_file(&self) -> Option<(&File, FileSize)> {
        match self {
            Self::AppendVec(av) => Some((av.file(), av.len() as FileSize)),
            Self::Split(split) => split
                .data_file()
                .map(|data_file| (data_file, split.data_len())),
        }
    }

    /// Returns a file handle for a bulk read (one sequential pass over the whole file) in batch
    /// work like archiving or index generation, rather than for account loads.
    /// With `use_direct_io = true` a fresh fd is opened with `O_DIRECT`; otherwise the
    /// `AccountsFile`'s existing fd is borrowed.
    /// Returns `None` for a split file with no external account data file.
    pub fn open_file_for_bulk_read(
        &self,
        use_direct_io: bool,
    ) -> io::Result<Option<OpenFileForBulkRead<'_>>> {
        let Some((data_file, read_limit)) = self.account_data_file() else {
            return Ok(None);
        };
        let file = if use_direct_io {
            let path = match self {
                Self::AppendVec(av) => av.path(),
                Self::Split(split) => {
                    // SAFETY: A data file implies that its path is present as well.
                    split.data_path().unwrap()
                }
            };
            BulkReadFile::Owned(open_for_reading(path, true)?)
        } else {
            BulkReadFile::Borrowed(data_file)
        };
        Ok(Some(OpenFileForBulkRead { file, read_limit }))
    }
}

/// An enum that creates AccountsFile instance with the specified format.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum AccountsFileProvider {
    #[default]
    AppendVec,
    Split,
}

impl AccountsFileProvider {
    pub fn new_writable(&self, path: impl Into<PathBuf>, file_size: u64) -> Result<AccountsFile> {
        Ok(match self {
            Self::AppendVec => AccountsFile::AppendVec(AppendVec::new(path, file_size as usize)),
            Self::Split => AccountsFile::Split(SplitFile::new(path.into())?),
        })
    }
}

/// The access method to use when bulk-reading an AccountsFile
#[derive(Debug)]
enum BulkReadFile<'a> {
    /// Borrowed `AccountsFile` fd; lacks `O_DIRECT`, so reads go through the
    /// kernel page cache (incompatible with direct-I/O reads).
    Borrowed(&'a File),
    /// Freshly opened fd, typically with `O_DIRECT` on Linux.
    Owned(File),
}

impl AsRef<File> for OpenFileForBulkRead<'_> {
    fn as_ref(&self) -> &File {
        match &self.file {
            BulkReadFile::Borrowed(f) => f,
            BulkReadFile::Owned(f) => f,
        }
    }
}

/// The account data file of an AccountsFile, opened for a bulk read
#[derive(Debug)]
pub struct OpenFileForBulkRead<'a> {
    file: BulkReadFile<'a>,
    read_limit: FileSize,
}

impl OpenFileForBulkRead<'_> {
    /// Returns the length of the account data (excludes a split file's metadata).
    pub fn read_limit(&self) -> FileSize {
        self.read_limit
    }
}

/// Information after storing accounts
#[derive(Debug)]
pub struct StoredAccountsInfo {
    /// offset in the storage where each account was stored
    pub offsets: Vec<Offset>,
    /// total size of all the stored accounts
    pub size: usize,
}

/// Creates a reusable buffered reader tuned for scanning storages.
pub(crate) fn new_scan_accounts_reader<'a>() -> impl RequiredLenBufFileRead<'a> {
    // 128KiB covers a reasonably large distribution of typical account sizes.
    // In a recent sample, 99.98% of accounts' data lengths were less than or equal to 128KiB.
    const MIN_CAPACITY: usize = 128 * 1024;

    // The max capacity is sized for the largest account plus the maximum metadata
    // from any of the account storage file formats.  Rounded up to the next page
    // size, since we're not gonna read less than a page anyway.  Both AppendVec
    // and SplitFile metadata is less than a page, so use 4KiB as the size.
    const MAX_CAPACITY: usize = 4096 + MAX_PERMITTED_DATA_LENGTH as usize;
    const _: () = assert!(MAX_CAPACITY.is_multiple_of(4096));

    const BUFFER_SIZE: usize = 32 * 1024;
    BufReaderWithOverflow::new(
        BufferedReader::<BUFFER_SIZE>::new(),
        MIN_CAPACITY,
        MAX_CAPACITY,
    )
}

#[cfg(test)]
mod tests {
    use {super::*, solana_account::accounts_equal, std::fs, tempfile::TempDir};

    #[test]
    fn test_new_split_from_append_vec() {
        let temp_dir = TempDir::new().unwrap();
        let append_vec_path = temp_dir.path().join("12.34");
        let append_vec = AppendVec::new(&append_vec_path, 50_000);

        let owner = Pubkey::default();
        let accounts = [
            (Pubkey::new_unique(), AccountSharedData::new(1, 0, &owner)),
            (Pubkey::new_unique(), AccountSharedData::new(0, 0, &owner)),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(33, 10_000, &owner),
            ),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(44, 222, &owner),
            ),
        ];
        let dummy_slot = 12; // value isn't used
        append_vec
            .append_accounts(&(dummy_slot, accounts.as_slice()))
            .unwrap();

        let append_vec_file_info = FileInfo::new_from_path(&append_vec_path).unwrap();
        let split = AccountsFile::new_split_from_append_vec(&append_vec_file_info).unwrap();

        let mut reader = new_scan_accounts_reader();
        split.set_file_on_reader(&mut reader).unwrap();
        let mut i = 0;
        split
            .scan_accounts_with(&mut reader, |_offset, stored_account| {
                let (expected_pubkey, expected_account) = &accounts[i];
                assert_eq!(stored_account.pubkey(), expected_pubkey);
                assert!(accounts_equal(&stored_account, expected_account));
                i += 1;
            })
            .unwrap();
        assert_eq!(i, accounts.len());
    }

    #[test]
    fn test_open_append_vec_for_startup_converts_large_append_vec() {
        let temp_dir = tempfile::tempdir().unwrap();
        let append_vec_path = temp_dir.path().join("56.78");

        let append_vec_file = File::create_new(&append_vec_path).unwrap();
        append_vec_file
            .set_len(MAXIMUM_APPEND_VEC_FILE_SIZE + 1)
            .unwrap();

        let append_vec_file_info = FileInfo::new_from_path(&append_vec_path).unwrap();
        let accounts_file =
            AccountsFile::open_append_vec_for_startup(append_vec_file_info).unwrap();

        assert!(matches!(accounts_file, AccountsFile::Split(_)));
        assert!(!fs::exists(&append_vec_path).unwrap());
    }

    #[test]
    fn test_open_append_vec_for_startup_keeps_small_append_vec() {
        let temp_dir = tempfile::tempdir().unwrap();
        let append_vec_path = temp_dir.path().join("80.8");

        let append_vec = AppendVec::new(&append_vec_path, MAXIMUM_APPEND_VEC_FILE_SIZE as usize);
        append_vec.disable_remove_on_drop();
        drop(append_vec);

        let append_vec_file_info = FileInfo::new_from_path(&append_vec_path).unwrap();
        let accounts_file =
            AccountsFile::open_append_vec_for_startup(append_vec_file_info).unwrap();

        assert!(matches!(accounts_file, AccountsFile::AppendVec(_)));
        assert!(fs::exists(&append_vec_path).unwrap());
    }
}
