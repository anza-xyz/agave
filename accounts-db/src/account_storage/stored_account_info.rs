use {solana_account::ReadableAccount, solana_clock::Epoch, solana_pubkey::Pubkey};

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
    pub fn lamports(&self) -> u64 {
        self.lamports
    }
    pub fn owner(&self) -> &Pubkey {
        self.owner
    }
    pub fn data_len(&self) -> usize {
        self.data_len
    }
    pub fn executable(&self) -> bool {
        self.executable
    }
    pub fn rent_epoch(&self) -> Epoch {
        self.rent_epoch
    }
}

impl<'storage> StoredAccountInfoWithoutData<'storage> {
    /// Constructs a new StoredAccountInfoWithoutData from a StoredAccountInfo
    ///
    /// Use this ctor when `stored_account_info` is going out of scope, *but not* the underlying
    /// `'storage`.  This facilitates incremental improvements towards not reading account data
    /// unnecessarily, by changing out the front-end code separately from the back-end.
    pub fn new_from<'other>(stored_account_info: &'other StoredAccountInfo<'storage>) -> Self {
        // Note that we must use the pubkey/owner fields directly so that we can get the `'storage`
        // lifetime of `stored_account_info`, and *not* its `'other` lifetime.
        Self {
            pubkey: stored_account_info.pubkey,
            lamports: stored_account_info.lamports,
            owner: stored_account_info.owner,
            data_len: stored_account_info.data.len(),
            executable: stored_account_info.executable,
            rent_epoch: stored_account_info.rent_epoch,
        }
    }
}
