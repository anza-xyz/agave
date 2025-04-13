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
