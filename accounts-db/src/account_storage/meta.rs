use {
    crate::{
        accounts_hash::AccountHash,
        append_vec::AppendVecStoredAccountMeta,
        storable_accounts::StorableAccounts,
        tiered_storage::hot::{HotAccount, HotAccountMeta},
    },
    solana_sdk::{account::ReadableAccount, hash::Hash, pubkey::Pubkey, stake_history::Epoch},
    std::{borrow::Borrow, marker::PhantomData},
};

pub type StoredMetaWriteVersion = u64;
// A tuple that stores offset and size respectively
#[derive(Debug, Clone)]
pub struct StoredAccountInfo {
    pub offset: usize,
    pub size: usize,
}

lazy_static! {
    static ref DEFAULT_ACCOUNT_HASH: AccountHash = AccountHash(Hash::default());
}

/// Goal is to eliminate copies and data reshaping given various code paths that store accounts.
/// This struct contains what is needed to store accounts to a storage
/// 1. account & pubkey (StorableAccounts)
/// 2. hash per account (Maybe in StorableAccounts, otherwise has to be passed in separately)
pub struct StorableAccountsWithHashes<
    'a: 'b,
    'b,
    T: ReadableAccount + Sync + 'b,
    U: StorableAccounts<'a, T>,
    V: Borrow<AccountHash>,
> {
    /// accounts to store
    /// always has pubkey and account
    /// may also have hash per account
    pub(crate) accounts: &'b U,
    /// if accounts does not have hash, this has a hash per account
    hashes: Option<Vec<V>>,
    _phantom: PhantomData<&'a T>,
}

impl<
        'a: 'b,
        'b,
        T: ReadableAccount + Sync + 'b,
        U: StorableAccounts<'a, T>,
        V: Borrow<AccountHash>,
    > StorableAccountsWithHashes<'a, 'b, T, U, V>
{
    /// used when accounts contains hash already
    pub fn new(accounts: &'b U) -> Self {
        assert!(accounts.has_hash());
        Self {
            accounts,
            hashes: None,
            _phantom: PhantomData,
        }
    }
    /// used when accounts does NOT contains hash
    /// In this case, hashes have to be passed in separately.
    pub fn new_with_hashes(accounts: &'b U, hashes: Vec<V>) -> Self {
        assert!(!accounts.has_hash());
        assert_eq!(accounts.len(), hashes.len());
        Self {
            accounts,
            hashes: Some(hashes),
            _phantom: PhantomData,
        }
    }

    /// get all account fields at 'index'
    pub fn get(&self, index: usize) -> (Option<&T>, &Pubkey, &AccountHash) {
        let account = self.accounts.account_default_if_zero_lamport(index);
        let pubkey = self.accounts.pubkey(index);
        let hash = if self.accounts.has_hash() {
            self.accounts.hash(index)
        } else {
            let item = self.hashes.as_ref().unwrap();
            item[index].borrow()
        };
        (account, pubkey, hash)
    }

    /// None if account at index has lamports == 0
    /// Otherwise, Some(account)
    /// This is the only way to access the account.
    pub fn account(&self, index: usize) -> Option<&T> {
        self.accounts.account_default_if_zero_lamport(index)
    }

    /// # accounts to write
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// References to account data stored elsewhere. Getting an `Account` requires cloning
/// (see `StoredAccountMeta::clone_account()`).
#[derive(PartialEq, Eq, Debug)]
pub enum StoredAccountMeta<'storage> {
    AppendVec(AppendVecStoredAccountMeta<'storage>),
    Hot(HotAccount<'storage, HotAccountMeta>),
}

impl<'storage> StoredAccountMeta<'storage> {
    pub fn pubkey(&self) -> &'storage Pubkey {
        match self {
            Self::AppendVec(av) => av.pubkey(),
            Self::Hot(hot) => hot.address(),
        }
    }

    pub fn hash(&self) -> &'storage AccountHash {
        match self {
            Self::AppendVec(av) => av.hash(),
            // tiered-storage has deprecated the use of AccountHash
            Self::Hot(_) => &DEFAULT_ACCOUNT_HASH,
        }
    }

    pub fn stored_size(&self) -> usize {
        match self {
            Self::AppendVec(av) => av.stored_size(),
            Self::Hot(_) => unimplemented!(),
        }
    }

    pub fn offset(&self) -> usize {
        match self {
            Self::AppendVec(av) => av.offset(),
            Self::Hot(hot) => hot.index().0 as usize,
        }
    }

    pub fn data(&self) -> &'storage [u8] {
        match self {
            Self::AppendVec(av) => av.data(),
            Self::Hot(hot) => hot.data(),
        }
    }

    pub fn data_len(&self) -> u64 {
        match self {
            Self::AppendVec(av) => av.data_len(),
            Self::Hot(hot) => hot.data().len() as u64,
        }
    }

    pub fn write_version(&self) -> StoredMetaWriteVersion {
        match self {
            Self::AppendVec(av) => av.write_version(),
            // Hot account does not support this API as it does not
            // use a write version.
            Self::Hot(_) => StoredMetaWriteVersion::default(),
        }
    }

    pub fn meta(&self) -> &StoredMeta {
        match self {
            Self::AppendVec(av) => av.meta(),
            // Hot account does not support this API as it does not
            // use the same in-memory layout as StoredMeta.
            Self::Hot(_) => unreachable!(),
        }
    }

    pub fn set_meta(&mut self, meta: &'storage StoredMeta) {
        match self {
            Self::AppendVec(av) => av.set_meta(meta),
            // Hot account does not support this API as it does not
            // use the same in-memory layout as StoredMeta.
            Self::Hot(_) => unreachable!(),
        }
    }

    pub(crate) fn sanitize(&self) -> bool {
        match self {
            Self::AppendVec(av) => av.sanitize(),
            // Hot account currently doesn't have the concept of sanitization.
            Self::Hot(_) => unimplemented!(),
        }
    }
}

impl<'storage> ReadableAccount for StoredAccountMeta<'storage> {
    fn lamports(&self) -> u64 {
        match self {
            Self::AppendVec(av) => av.lamports(),
            Self::Hot(hot) => hot.lamports(),
        }
    }
    fn data(&self) -> &[u8] {
        match self {
            Self::AppendVec(av) => av.data(),
            Self::Hot(hot) => hot.data(),
        }
    }
    fn owner(&self) -> &Pubkey {
        match self {
            Self::AppendVec(av) => av.owner(),
            Self::Hot(hot) => hot.owner(),
        }
    }
    fn executable(&self) -> bool {
        match self {
            Self::AppendVec(av) => av.executable(),
            Self::Hot(hot) => hot.executable(),
        }
    }
    fn rent_epoch(&self) -> Epoch {
        match self {
            Self::AppendVec(av) => av.rent_epoch(),
            Self::Hot(hot) => hot.rent_epoch(),
        }
    }
}

/// Meta contains enough context to recover the index from storage itself
/// This struct will be backed by mmaped and snapshotted data files.
/// So the data layout must be stable and consistent across the entire cluster!
#[derive(Clone, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct StoredMeta {
    /// global write version
    /// This will be made completely obsolete such that we stop storing it.
    /// We will not support multiple append vecs per slot anymore, so this concept is no longer necessary.
    /// Order of stores of an account to an append vec will determine 'latest' account data per pubkey.
    pub write_version_obsolete: StoredMetaWriteVersion,
    pub data_len: u64,
    /// key for the account
    pub pubkey: Pubkey,
}

/// This struct will be backed by mmaped and snapshotted data files.
/// So the data layout must be stable and consistent across the entire cluster!
#[derive(Serialize, Deserialize, Clone, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct AccountMeta {
    /// lamports in the account
    pub lamports: u64,
    /// the epoch at which this account will next owe rent
    pub rent_epoch: Epoch,
    /// the program that owns this account. If executable, the program that loads this account.
    pub owner: Pubkey,
    /// this account's data contains a loaded program (and is now read-only)
    pub executable: bool,
}

impl<'a, T: ReadableAccount> From<&'a T> for AccountMeta {
    fn from(account: &'a T) -> Self {
        Self {
            lamports: account.lamports(),
            owner: *account.owner(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
        }
    }
}

impl<'a, T: ReadableAccount> From<Option<&'a T>> for AccountMeta {
    fn from(account: Option<&'a T>) -> Self {
        match account {
            Some(account) => AccountMeta::from(account),
            None => AccountMeta::default(),
        }
    }
}
