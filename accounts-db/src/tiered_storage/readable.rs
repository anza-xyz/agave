use {
    crate::{
        account_storage::{
            meta::StoredAccountMeta,
            stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        },
        accounts_file::MatchAccountOwnerError,
        append_vec::{IndexInfo, IndexInfoInner},
        tiered_storage::{
            file::TieredReadableFile,
            footer::{AccountMetaFormat, TieredStorageFooter},
            hot::HotStorageReader,
            index::IndexOffset,
            TieredStorageResult,
        },
    },
    solana_account::{AccountSharedData, ReadableAccount},
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

    /// Returns the `IndexInfo` for the account located at the specified index offset.
    ///
    /// Only intended to be used with the accounts index.
    pub(crate) fn get_account_index_info(
        &self,
        index_offset: IndexOffset,
    ) -> TieredStorageResult<Option<IndexInfo>> {
        self.get_stored_account_meta_callback(index_offset, |account| IndexInfo {
            stored_size_aligned: account.stored_size(),
            index_info: IndexInfoInner {
                pubkey: *account.pubkey(),
                lamports: account.lamports(),
                offset: account.offset(),
                data_len: account.data_len() as u64,
                executable: account.executable(),
                rent_epoch: account.rent_epoch(),
            },
        })
    }

    /// calls `callback` with the account located at the specified index offset.
    pub fn get_stored_account_callback<Ret>(
        &self,
        index_offset: IndexOffset,
        callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> TieredStorageResult<Option<Ret>> {
        match self {
            Self::Hot(hot) => hot.get_stored_account_callback(index_offset, callback),
        }
    }

    /// calls `callback` with the account located at the specified index offset.
    ///
    /// Prefer get_stored_account_callback() when possible, as it does not contain file format
    /// implementation details, and thus potentially can read less and be faster.
    pub fn get_stored_account_meta_callback<Ret>(
        &self,
        index_offset: IndexOffset,
        callback: impl for<'local> FnMut(StoredAccountMeta<'local>) -> Ret,
    ) -> TieredStorageResult<Option<Ret>> {
        match self {
            Self::Hot(hot) => hot.get_stored_account_meta_callback(index_offset, callback),
        }
    }

    /// Returns Ok(index_of_matching_owner) if the account owner at
    /// `account_offset` is one of the pubkeys in `owners`.
    ///
    /// Returns Err(MatchAccountOwnerError::NoMatch) if the account has 0
    /// lamports or the owner is not one of the pubkeys in `owners`.
    ///
    /// Returns Err(MatchAccountOwnerError::UnableToLoad) if there is any internal
    /// error that causes the data unable to load, including `account_offset`
    /// causes a data overrun.
    pub fn account_matches_owners(
        &self,
        index_offset: IndexOffset,
        owners: &[Pubkey],
    ) -> Result<usize, MatchAccountOwnerError> {
        match self {
            Self::Hot(hot) => {
                let account_offset = hot
                    .get_account_offset(index_offset)
                    .map_err(|_| MatchAccountOwnerError::UnableToLoad)?;
                hot.account_matches_owners(account_offset, owners)
            }
        }
    }

    /// iterate over all pubkeys
    pub fn scan_pubkeys(&self, callback: impl FnMut(&Pubkey)) -> TieredStorageResult<()> {
        match self {
            Self::Hot(hot) => hot.scan_pubkeys(callback),
        }
    }

    /// iterate over all entries to put in index
    pub(crate) fn scan_index(&self, callback: impl FnMut(IndexInfo)) -> TieredStorageResult<()> {
        match self {
            Self::Hot(hot) => hot.scan_index(callback),
        }
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// Note that account data is not read/passed to the callback.
    pub fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>),
    ) -> TieredStorageResult<()> {
        // Note, this should be reimplemented to not read account data
        self.scan_accounts(|stored_account| {
            let account = StoredAccountInfoWithoutData {
                pubkey: stored_account.pubkey(),
                lamports: stored_account.lamports(),
                owner: stored_account.owner(),
                data_len: stored_account.data().len(),
                executable: stored_account.executable(),
                rent_epoch: stored_account.rent_epoch(),
            };
            callback(account);
        })
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// Prefer scan_accounts_without_data() when account data is not needed,
    /// as it can potentially read less and be faster.
    pub fn scan_accounts(
        &self,
        mut callback: impl for<'local> FnMut(StoredAccountInfo<'local>),
    ) -> TieredStorageResult<()> {
        self.scan_accounts_stored_meta(|stored_account_meta| {
            let account = StoredAccountInfo {
                pubkey: stored_account_meta.pubkey(),
                lamports: stored_account_meta.lamports(),
                owner: stored_account_meta.owner(),
                data: stored_account_meta.data(),
                executable: stored_account_meta.executable(),
                rent_epoch: stored_account_meta.rent_epoch(),
            };
            callback(account);
        })
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// Prefer scan_accounts() when possible, as it does not contain file format
    /// implementation details, and thus potentially can read less and be faster.
    pub(crate) fn scan_accounts_stored_meta(
        &self,
        callback: impl for<'local> FnMut(StoredAccountMeta<'local>),
    ) -> TieredStorageResult<()> {
        match self {
            Self::Hot(hot) => hot.scan_accounts(callback),
        }
    }

    /// for each offset in `sorted_offsets`, return the account size
    pub(crate) fn get_account_sizes(
        &self,
        sorted_offsets: &[usize],
    ) -> TieredStorageResult<Vec<usize>> {
        match self {
            Self::Hot(hot) => hot.get_account_sizes(sorted_offsets),
        }
    }

    /// Returns a slice suitable for use when archiving tiered storages
    pub fn data_for_archive(&self) -> &[u8] {
        match self {
            Self::Hot(hot) => hot.data_for_archive(),
        }
    }
}
