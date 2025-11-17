use {
    crate::{
        account_info::{AccountInfo, Offset},
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_db::AccountsFileId,
        append_vec::{AppendVec, AppendVecError},
        storable_accounts::StorableAccounts,
        tiered_storage::{
            error::TieredStorageError, hot::HOT_FORMAT, index::IndexOffset, TieredStorage,
        },
    },
    agave_fs::buffered_reader::RequiredLenBufFileRead,
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
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
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("AppendVecError: {0}")]
    AppendVecError(#[from] AppendVecError),

    #[error("TieredStorageError: {0}")]
    TieredStorageError(#[from] TieredStorageError),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StorageAccess {
    /// storages should be accessed by Mmap
    #[deprecated(since = "4.0.0")]
    Mmap,
    /// storages should be accessed by File I/O
    /// ancient storages are created by 1-shot write to pack multiple accounts together more efficiently with new formats
    #[default]
    File,
}

#[derive(Debug)]
/// An enum for accessing an accounts file which can be implemented
/// under different formats.
pub enum AccountsFile {
    AppendVec(AppendVec),
    TieredStorage(TieredStorage),
}

impl AccountsFile {
    /// Create an AccountsFile instance from the specified path.
    ///
    /// The second element of the returned tuple is the number of accounts in the
    /// accounts file.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_from_file(
        path: impl Into<PathBuf>,
        current_len: usize,
        storage_access: StorageAccess,
    ) -> Result<(Self, usize)> {
        let (av, num_accounts) = AppendVec::new_from_file(path, current_len, storage_access)?;
        Ok((Self::AppendVec(av), num_accounts))
    }

    /// Creates a new AccountsFile for the underlying storage at `path`
    ///
    /// This version of `new()` may only be called when reconstructing storages as part of startup.
    /// It trusts the snapshot's value for `current_len`, and relies on later index generation or
    /// accounts verification to ensure it is valid.
    pub fn new_for_startup(
        path: impl Into<PathBuf>,
        current_len: usize,
        storage_access: StorageAccess,
    ) -> Result<Self> {
        let av = AppendVec::new_for_startup(path, current_len, storage_access)?;
        Ok(Self::AppendVec(av))
    }

    /// if storage is not readonly, reopen another instance that is read only
    pub(crate) fn reopen_as_readonly(&self) -> Option<Self> {
        match self {
            Self::AppendVec(av) => av.reopen_as_readonly_file_io().map(Self::AppendVec),
            Self::TieredStorage(_) => None,
        }
    }

    /// Return the total number of bytes of the zero lamport single ref accounts in the storage.
    /// Those bytes are "dead" and can be shrunk away.
    pub(crate) fn dead_bytes_due_to_zero_lamport_single_ref(&self, count: usize) -> usize {
        match self {
            Self::AppendVec(av) => av.dead_bytes_due_to_zero_lamport_single_ref(count),
            Self::TieredStorage(ts) => ts.dead_bytes_due_to_zero_lamport_single_ref(count),
        }
    }

    /// Flushes contents to disk
    pub fn flush(&self) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.flush()?,
            Self::TieredStorage(_) => {}
        }
        Ok(())
    }

    pub fn remaining_bytes(&self) -> u64 {
        match self {
            Self::AppendVec(av) => av.remaining_bytes(),
            Self::TieredStorage(ts) => ts.capacity().saturating_sub(ts.len() as u64),
        }
    }

    /// Returns the number of bytes, *not accounts*, used in the AccountsFile
    pub fn len(&self) -> usize {
        match self {
            Self::AppendVec(av) => av.len(),
            Self::TieredStorage(ts) => ts.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::AppendVec(av) => av.is_empty(),
            Self::TieredStorage(ts) => ts.is_empty(),
        }
    }

    /// Returns the total number of bytes, *not accounts*, the AccountsFile can hold
    pub fn capacity(&self) -> u64 {
        match self {
            Self::AppendVec(av) => av.capacity(),
            Self::TieredStorage(ts) => ts.capacity(),
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
        offset: usize,
        callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Option<Ret> {
        match self {
            Self::AppendVec(av) => av.get_stored_account_without_data_callback(offset, callback),
            Self::TieredStorage(ts) => {
                // Note: The conversion here is needed as the AccountsDB currently
                // assumes all offsets are multiple of 8 while TieredStorage uses
                // IndexOffset that is equivalent to AccountInfo::reduced_offset.
                let index_offset = IndexOffset(AccountInfo::get_reduced_offset(offset));
                ts.reader()?
                    .get_stored_account_without_data_callback(index_offset, callback)
                    .ok()?
            }
        }
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
        offset: usize,
        callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Option<Ret> {
        match self {
            Self::AppendVec(av) => av.get_stored_account_callback(offset, callback),
            Self::TieredStorage(ts) => {
                // Note: The conversion here is needed as the AccountsDB currently
                // assumes all offsets are multiple of 8 while TieredStorage uses
                // IndexOffset that is equivalent to AccountInfo::reduced_offset.
                let index_offset = IndexOffset(AccountInfo::get_reduced_offset(offset));
                ts.reader()?
                    .get_stored_account_callback(index_offset, callback)
                    .ok()?
            }
        }
    }

    /// return an `AccountSharedData` for an account at `offset`, if any.  Otherwise return None.
    pub(crate) fn get_account_shared_data(&self, offset: usize) -> Option<AccountSharedData> {
        match self {
            Self::AppendVec(av) => av.get_account_shared_data(offset),
            Self::TieredStorage(ts) => {
                // Note: The conversion here is needed as the AccountsDB currently
                // assumes all offsets are multiple of 8 while TieredStorage uses
                // IndexOffset that is equivalent to AccountInfo::reduced_offset.
                let index_offset = IndexOffset(AccountInfo::get_reduced_offset(offset));
                ts.reader()?.get_account_shared_data(index_offset).ok()?
            }
        }
    }

    /// Return the path of the underlying account file.
    pub fn path(&self) -> &Path {
        match self {
            Self::AppendVec(av) => av.path(),
            Self::TieredStorage(ts) => ts.path(),
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
        callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_accounts_without_data(callback)?,
            Self::TieredStorage(ts) => {
                if let Some(reader) = ts.reader() {
                    reader.scan_accounts_without_data(callback)?;
                }
            }
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
    pub(crate) fn scan_accounts<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_accounts(reader, callback)?,
            Self::TieredStorage(ts) => {
                if let Some(reader) = ts.reader() {
                    reader.scan_accounts(callback)?;
                }
            }
        }
        Ok(())
    }

    /// Calculate the amount of storage required for an account with the passed
    /// in data_len
    pub(crate) fn calculate_stored_size(&self, data_len: usize) -> usize {
        match self {
            Self::AppendVec(_) => AppendVec::calculate_stored_size(data_len),
            Self::TieredStorage(ts) => ts
                .reader()
                .expect("Reader must be initialized as stored size is specific to format")
                .calculate_stored_size(data_len),
        }
    }

    /// for each offset in `sorted_offsets`, get the data size
    pub(crate) fn get_account_data_lens(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        match self {
            Self::AppendVec(av) => av.get_account_data_lens(sorted_offsets),
            Self::TieredStorage(ts) => ts
                .reader()
                .and_then(|reader| reader.get_account_data_lens(sorted_offsets).ok())
                .unwrap_or_default(),
        }
    }

    /// iterate over all pubkeys
    pub fn scan_pubkeys(&self, callback: impl FnMut(&Pubkey)) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_pubkeys(callback)?,
            Self::TieredStorage(ts) => {
                if let Some(reader) = ts.reader() {
                    reader.scan_pubkeys(callback)?;
                }
            }
        }
        Ok(())
    }

    /// Copy each account metadata, account and hash to the internal buffer.
    /// If there is no room to write the first entry, None is returned.
    /// Otherwise, returns the starting offset of each account metadata.
    /// Plus, the final return value is the offset where the next entry would be appended.
    /// So, return.len() is 1 + (number of accounts written)
    /// After each account is appended, the internal `current_len` is updated
    /// and will be available to other threads.
    pub fn write_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
    ) -> Option<StoredAccountsInfo> {
        match self {
            Self::AppendVec(av) => av.append_accounts(accounts, skip),
            // Note: The conversion here is needed as the AccountsDB currently
            // assumes all offsets are multiple of 8 while TieredStorage uses
            // IndexOffset that is equivalent to AccountInfo::reduced_offset.
            Self::TieredStorage(ts) => ts
                .write_accounts(accounts, skip, &HOT_FORMAT)
                .map(|mut stored_accounts_info| {
                    stored_accounts_info.offsets.iter_mut().for_each(|offset| {
                        *offset = AccountInfo::reduced_offset_to_offset(*offset as u32);
                    });
                    stored_accounts_info
                })
                .ok(),
        }
    }

    /// Returns the way to access this accounts file when archiving
    pub fn internals_for_archive(&self) -> InternalsForArchive<'_> {
        match self {
            Self::AppendVec(av) => av.internals_for_archive(),
            Self::TieredStorage(ts) => InternalsForArchive::Mmap(
                ts.reader()
                    .expect("must be a reader when archiving")
                    .data_for_archive(),
            ),
        }
    }
}

/// An enum that creates AccountsFile instance with the specified format.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum AccountsFileProvider {
    #[default]
    AppendVec,
    HotStorage,
}

impl AccountsFileProvider {
    pub fn new_writable(
        &self,
        path: impl Into<PathBuf>,
        file_size: u64,
        storage_access: StorageAccess,
    ) -> AccountsFile {
        match self {
            Self::AppendVec => AccountsFile::AppendVec(AppendVec::new(
                path,
                true,
                file_size as usize,
                storage_access,
            )),
            Self::HotStorage => AccountsFile::TieredStorage(TieredStorage::new_writable(path)),
        }
    }
}

/// The access method to use when archiving an AccountsFile
#[derive(Debug)]
pub enum InternalsForArchive<'a> {
    /// Accessing the internals is done via Mmap
    Mmap(&'a [u8]),
    /// Accessing the internals is done via File I/O
    FileIo(&'a Path),
}

/// Information after storing accounts
#[derive(Debug)]
pub struct StoredAccountsInfo {
    /// offset in the storage where each account was stored
    pub offsets: Vec<usize>,
    /// total size of all the stored accounts
    pub size: usize,
}
