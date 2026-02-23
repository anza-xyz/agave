use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        tiered_storage::{
            TieredStorageResult,
            file::TieredReadableFile,
            footer::{AccountMetaFormat, TieredStorageFooter},
            hot::HotStorageReader,
            index::IndexOffset,
        },
    },
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
    std::path::Path,
};

/// The reader of a tiered storage instance.
#[derive(Debug)]
pub enum TieredStorageReader {
    Hot(HotStorageReader),
}

impl TieredStorageReader {
    /// Creates a reader for the specified tiered storage accounts file.
    pub fn new_from_path(path: impl AsRef<Path>) -> TieredStorageResult<Self> {
        let file = TieredReadableFile::new(&path)?;
        let footer = TieredStorageFooter::new_from_footer_block(&file)?;
        match footer.account_meta_format {
            AccountMetaFormat::Hot => Ok(Self::Hot(HotStorageReader::new(file)?)),
        }
    }

    /// Returns the size of the underlying storage.
    pub fn len(&self) -> usize {
        match self {
            Self::Hot(hot) => hot.len(),
        }
    }

    /// Returns whether the underlying storage is empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Hot(hot) => hot.is_empty(),
        }
    }

    pub fn capacity(&self) -> u64 {
        match self {
            Self::Hot(hot) => hot.capacity(),
        }
    }

    /// Returns the footer of the associated HotAccountsFile.
    pub fn footer(&self) -> &TieredStorageFooter {
        match self {
            Self::Hot(hot) => hot.footer(),
        }
    }

    /// Returns the total number of accounts.
    pub fn num_accounts(&self) -> usize {
        match self {
            Self::Hot(hot) => hot.num_accounts(),
        }
    }

    /// Returns the account located at the specified index offset.
    pub fn get_account_shared_data(
        &self,
        index_offset: IndexOffset,
    ) -> TieredStorageResult<Option<AccountSharedData>> {
        match self {
            Self::Hot(hot) => hot.get_account_shared_data(index_offset),
        }
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
        index_offset: IndexOffset,
        callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> TieredStorageResult<Option<Ret>> {
        match self {
            Self::Hot(hot) => hot.get_stored_account_without_data_callback(index_offset, callback),
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
        index_offset: IndexOffset,
        callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> TieredStorageResult<Option<Ret>> {
        match self {
            Self::Hot(hot) => hot.get_stored_account_callback(index_offset, callback),
        }
    }

    /// iterate over all pubkeys
    pub fn scan_pubkeys(&self, callback: impl FnMut(&Pubkey)) -> TieredStorageResult<()> {
        match self {
            Self::Hot(hot) => hot.scan_pubkeys(callback),
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
    ) -> TieredStorageResult<()> {
        match self {
            Self::Hot(hot) => hot.scan_accounts_without_data(callback),
        }
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfo: the account itself, with account data
    ///
    /// Prefer scan_accounts_without_data() when account data is not needed,
    /// as it can potentially read less and be faster.
    pub fn scan_accounts(
        &self,
        callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> TieredStorageResult<()> {
        match self {
            Self::Hot(hot) => hot.scan_accounts(callback),
        }
    }

    /// Calculate the amount of storage required for an account with the passed
    /// in data_len
    pub(crate) fn calculate_stored_size(&self, data_len: usize) -> usize {
        match self {
            Self::Hot(_) => HotStorageReader::calculate_stored_size(data_len),
        }
    }

    /// for each offset in `sorted_offsets`, return the length of data stored in the account
    pub(crate) fn get_account_data_lens(
        &self,
        sorted_offsets: &[usize],
    ) -> TieredStorageResult<Vec<usize>> {
        match self {
            Self::Hot(hot) => hot.get_account_data_lens(sorted_offsets),
        }
    }

    /// Returns a slice suitable for use when archiving tiered storages
    pub fn data_for_archive(&self) -> &[u8] {
        match self {
            Self::Hot(hot) => hot.data_for_archive(),
        }
    }
}
