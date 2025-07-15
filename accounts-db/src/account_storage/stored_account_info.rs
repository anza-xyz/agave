use {
    crate::is_zero_lamport::IsZeroLamport, solana_account::ReadableAccount, solana_clock::Epoch,
    solana_pubkey::Pubkey,
};

/// Account type with fields that reference into a storage
///
/// Used then scanning the accounts of a single storage.
#[derive(Debug, Clone)]
pub struct StoredAccountInfo<'storage> {
    pub pubkey: &'storage Pubkey,
    pub lamports: u64,
    pub owner: &'storage Pubkey,
    pub data: &'storage [u8],
    pub executable: bool,
    pub rent_epoch: Epoch,
}

impl StoredAccountInfo<'_> {
    pub fn pubkey(&self) -> &Pubkey {
        self.pubkey
    }
}

impl<'storage> StoredAccountInfo<'storage> {
    /// Constructs a new StoredAccountInfo from a StoredAccountInfoWithoutData and data
    ///
    /// Use this ctor when `other_stored_account` is going out of scope, *but not* the underlying
    /// `'storage`.  This facilitates incremental improvements towards not reading account data
    /// unnecessarily, by changing out the front-end code separately from the back-end.
    pub fn new_from<'other>(
        other_stored_account: &'other StoredAccountInfoWithoutData<'storage>,
        data: &'storage [u8],
    ) -> Self {
        // Note that we must use the pubkey/owner fields directly so that we can get the `'storage`
        // lifetime of `other_stored_account`, and *not* its `'other` lifetime.
        assert_eq!(other_stored_account.data_len, data.len());
        Self {
            pubkey: other_stored_account.pubkey,
            lamports: other_stored_account.lamports,
            owner: other_stored_account.owner,
            data,
            executable: other_stored_account.executable,
            rent_epoch: other_stored_account.rent_epoch,
        }
    }
}

impl ReadableAccount for StoredAccountInfo<'_> {
    fn lamports(&self) -> u64 {
        self.lamports
    }
    fn owner(&self) -> &Pubkey {
        self.owner
    }
    fn data(&self) -> &[u8] {
        self.data
    }
    fn executable(&self) -> bool {
        self.executable
    }
    fn rent_epoch(&self) -> Epoch {
        self.rent_epoch
    }
}

/// Account type with fields that reference into a storage, *without* data
///
/// Used then scanning the accounts of a single storage.
#[derive(Debug, Clone)]
pub struct StoredAccountInfoWithoutData<'storage> {
    pub pubkey: &'storage Pubkey,
    pub lamports: u64,
    pub owner: &'storage Pubkey,
    pub data_len: usize,
    pub executable: bool,
    pub rent_epoch: Epoch,
}

impl StoredAccountInfoWithoutData<'_> {
    pub fn pubkey(&self) -> &Pubkey {
        self.pubkey
    }
}

impl<'storage> StoredAccountInfoWithoutData<'storage> {
    /// Constructs a new StoredAccountInfoWithoutData from a StoredAccountInfo
    ///
    /// Use this ctor when `other_stored_account` is going out of scope, *but not* the underlying
    /// `'storage`.  This facilitates incremental improvements towards not reading account data
    /// unnecessarily, by changing out the front-end code separately from the back-end.
    pub fn new_from<'other>(other_stored_account: &'other StoredAccountInfo<'storage>) -> Self {
        // Note that we must use the pubkey/owner fields directly so that we can get the `'storage`
        // lifetime of `other_stored_account`, and *not* its `'other` lifetime.
        Self {
            pubkey: other_stored_account.pubkey,
            lamports: other_stored_account.lamports,
            owner: other_stored_account.owner,
            data_len: other_stored_account.data.len(),
            executable: other_stored_account.executable,
            rent_epoch: other_stored_account.rent_epoch,
        }
    }
}

impl IsZeroLamport for StoredAccountInfoWithoutData<'_> {
    fn is_zero_lamport(&self) -> bool {
        self.lamports == 0
    }
}
