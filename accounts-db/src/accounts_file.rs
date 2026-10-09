use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_db::AccountsFileId,
        append_vec::{AppendVec, AppendVecError},
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
        fs::File,
        io,
        iter::ExactSizeIterator,
        mem,
        path::{Path, PathBuf},
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
    /// Creates a new AccountsFile for the underlying storage at `file_info`
    ///
    /// This version of `new()` may only be called when reconstructing storages as part of startup.
    /// The storage length is taken to be the full file size; this is trusted and relies on later
    /// index generation or accounts verification to ensure it is valid.
    pub fn new_for_startup(file_info: FileInfo) -> Result<Self> {
        let av = AppendVec::new_for_startup(file_info)?;
        Ok(Self::AppendVec(av))
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
        let (file, read_limit) = self.account_data_file();
        reader.set_file(file, read_limit)
    }

    /// Scans the file already activated on the reader, preserving archive read-ahead and I/O mode.
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

    /// Returns the file holding the account data and its length; a split file without a data file
    /// returns its meta file with length 0.
    fn account_data_file(&self) -> (&File, FileSize) {
        match self {
            Self::AppendVec(av) => (av.file(), av.len() as FileSize),
            Self::Split(split) => match split.data_file() {
                Some(data_file) => (data_file, split.data_len()),
                None => (split.meta_file(), 0),
            },
        }
    }

    /// Returns a file handle suitable for archive-style reads. With
    /// `use_direct_io = true` a fresh fd is opened with `O_DIRECT`; otherwise
    /// the `AccountsFile`'s existing fd is borrowed, saving one fd per storage.
    pub fn open_file_for_archive(&self, use_direct_io: bool) -> io::Result<OpenFileForArchive<'_>> {
        let (data_file, read_limit) = self.account_data_file();
        let file = if use_direct_io {
            let path = match self {
                Self::AppendVec(av) => av.path(),
                Self::Split(split) => split.data_path().unwrap_or_else(|| {
                    // We're opening a file here to use with AccountStorageReader for archiving
                    // snapshots.  However, this SplitFile doesn't have a data file, so there's
                    // nothing to actually read...  Since we need to return something, using
                    // the meta file here is fine; the AccountStorageReader will never use it.
                    // Ideal? No.  Safe? Yes.
                    split.meta_path()
                }),
            };
            ArchiveFile::Owned(open_for_reading(path, true)?)
        } else {
            ArchiveFile::Borrowed(data_file)
        };
        Ok(OpenFileForArchive { file, read_limit })
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

/// The access method to use when archiving an AccountsFile
#[derive(Debug)]
enum ArchiveFile<'a> {
    /// Borrowed `AccountsFile` fd; lacks `O_DIRECT`, so reads go through the
    /// kernel page cache (incompatible with direct-I/O reads).
    Borrowed(&'a File),
    /// Freshly opened fd, typically with `O_DIRECT` on Linux.
    Owned(File),
}

impl AsRef<File> for OpenFileForArchive<'_> {
    fn as_ref(&self) -> &File {
        match &self.file {
            ArchiveFile::Borrowed(f) => f,
            ArchiveFile::Owned(f) => f,
        }
    }
}

/// The account data file of an AccountsFile, opened for archiving
#[derive(Debug)]
pub struct OpenFileForArchive<'a> {
    file: ArchiveFile<'a>,
    read_limit: FileSize,
}

impl OpenFileForArchive<'_> {
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
