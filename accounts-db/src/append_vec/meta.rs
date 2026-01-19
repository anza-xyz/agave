use {
    crate::is_zero_lamport::IsZeroLamport, solana_account::ReadableAccount, solana_clock::Epoch,
    solana_pubkey::Pubkey, std::ptr,
};

/// Meta contains enough context to recover the index from storage itself
/// This struct will be backed by mmapped and snapshotted data files.
/// So the data layout must be stable and consistent across the entire cluster!
#[derive(Clone, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct StoredMeta {
    /// global write version
    /// This will be made completely obsolete such that we stop storing it.
    /// We will not support multiple append vecs per slot anymore, so this concept is no longer necessary.
    /// Order of stores of an account to an append vec will determine 'latest' account data per pubkey.
    pub write_version_obsolete: u64,
    pub data_len: u64,
    /// key for the account
    pub pubkey: Pubkey,
}

/// This struct will be backed by mmapped and snapshotted data files.
/// So the data layout must be stable and consistent across the entire cluster!
#[derive(Clone, Debug, Eq, PartialEq)]
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

/// References to account data stored elsewhere. Getting an `Account` requires cloning
/// (see `StoredAccountMeta::clone_account()`).
#[derive(Debug)]
pub struct StoredAccountMeta<'append_vec> {
    pub meta: &'append_vec StoredMeta,
    /// account data
    pub account_meta: &'append_vec AccountMeta,
    pub(crate) data: &'append_vec [u8],
    pub(crate) offset: usize,
    pub(crate) stored_size: usize,
}

impl<'append_vec> StoredAccountMeta<'append_vec> {
    pub fn pubkey(&self) -> &'append_vec Pubkey {
        &self.meta.pubkey
    }

    pub fn stored_size(&self) -> usize {
        self.stored_size
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn data(&self) -> &'append_vec [u8] {
        self.data
    }
}

impl IsZeroLamport for StoredAccountMeta<'_> {
    fn is_zero_lamport(&self) -> bool {
        self.lamports() == 0
    }
}

impl<'append_vec> ReadableAccount for StoredAccountMeta<'append_vec> {
    fn lamports(&self) -> u64 {
        self.account_meta.lamports
    }
    fn data(&self) -> &'append_vec [u8] {
        self.data()
    }
    fn owner(&self) -> &'append_vec Pubkey {
        &self.account_meta.owner
    }
    fn executable(&self) -> bool {
        self.account_meta.executable
    }
    fn rent_epoch(&self) -> Epoch {
        self.account_meta.rent_epoch
    }
}

/// [`StoredAccountMeta`] without account data or account hash.
pub struct StoredAccountNoData<'append_vec> {
    pub meta: &'append_vec StoredMeta,
    pub account_meta: &'append_vec AccountMeta,
    pub offset: usize,
    pub stored_size: usize,
}

impl<'append_vec> StoredAccountNoData<'append_vec> {
    #[inline(always)]
    pub fn lamports(&self) -> u64 {
        self.account_meta.lamports
    }

    #[inline(always)]
    pub fn owner(&self) -> &'append_vec Pubkey {
        &self.account_meta.owner
    }

    #[inline(always)]
    pub fn pubkey(&self) -> &'append_vec Pubkey {
        &self.meta.pubkey
    }

    pub fn sanitize(&self) -> bool {
        self.sanitize_executable() && self.sanitize_lamports()
    }

    #[inline(always)]
    pub fn offset(&self) -> usize {
        self.offset
    }

    #[inline(always)]
    pub fn stored_size(&self) -> usize {
        self.stored_size
    }

    #[inline(always)]
    pub fn data_len(&self) -> u64 {
        self.meta.data_len
    }

    #[inline(always)]
    pub fn executable(&self) -> bool {
        self.account_meta.executable
    }

    #[inline(always)]
    pub fn rent_epoch(&self) -> Epoch {
        self.account_meta.rent_epoch
    }

    pub fn sanitize_executable(&self) -> bool {
        // Sanitize executable to ensure higher 7-bits are cleared correctly.
        self.ref_executable_byte() & !1 == 0
    }

    /// Check if the account data matches that of a default account.
    ///
    /// Note that we are not comparing against AccountSharedData::default() because we do not have access to the account data,
    /// so we compare data _length_ in lieu of actual data. This check otherwise identical to AccountSharedData::default().
    pub fn is_default_account(&self) -> bool {
        self.account_meta.lamports == 0
            && self.meta.data_len == 0
            && !self.account_meta.executable
            && self.account_meta.rent_epoch == Epoch::default()
            && self.account_meta.owner == Pubkey::default()
    }

    pub fn sanitize_lamports(&self) -> bool {
        // Check if the account data matches that of a default account if it has 0 lamports.
        self.account_meta.lamports != 0 || self.is_default_account()
    }

    pub fn ref_executable_byte(&self) -> &u8 {
        // Use extra references to avoid value silently clamped to 1 (=true) and 0 (=false)
        // Yes, this really happens; see test_new_from_file_crafted_executable
        let executable_bool: &bool = &self.account_meta.executable;
        let executable_bool_ptr = ptr::from_ref(executable_bool);
        // UNSAFE: Force to interpret mmap-backed bool as u8 to really read the actual memory content
        let executable_byte: &u8 = unsafe { &*(executable_bool_ptr.cast()) };
        executable_byte
    }
}

impl IsZeroLamport for StoredAccountNoData<'_> {
    fn is_zero_lamport(&self) -> bool {
        self.lamports() == 0
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::{accounts_equal, Account},
    };

    #[test]
    fn test_stored_readable_account() {
        let lamports = 1;
        let owner = Pubkey::new_unique();
        let executable = true;
        let rent_epoch = 2;
        let meta = StoredMeta {
            write_version_obsolete: 5,
            pubkey: Pubkey::new_unique(),
            data_len: 7,
        };
        let account_meta = AccountMeta {
            lamports,
            owner,
            executable,
            rent_epoch,
        };
        let data = Vec::new();
        let account = Account {
            lamports,
            owner,
            executable,
            rent_epoch,
            data: data.clone(),
        };
        let offset = 99 * size_of::<u64>(); // offset needs to be 8 byte aligned
        let stored_size = 101;
        let stored_account = StoredAccountMeta {
            meta: &meta,
            account_meta: &account_meta,
            data: &data,
            offset,
            stored_size,
        };
        assert!(accounts_equal(&account, &stored_account));
    }
}
