//! The read of an account's meta entry, shared by the accounts file formats

use {
    agave_fs::FileSize,
    solana_account::{Account, AccountSharedData},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    std::{mem::MaybeUninit, slice},
};

/// The size of a `MetaEntryBuffer`: the most a meta entry read is, in any format
pub const META_ENTRY_BUFFER_SIZE: usize = 4 * 1024;

/// Byte buffer used for reading meta entries.
///
/// This buffer is aligned to ensure it is safe to read
/// the meta entry fields after loading from disk.
#[repr(align(8))]
pub struct MetaEntryBuffer([MaybeUninit<u8>; META_ENTRY_BUFFER_SIZE]);

impl MetaEntryBuffer {
    pub fn new() -> Self {
        Self([MaybeUninit::uninit(); META_ENTRY_BUFFER_SIZE])
    }

    /// The buffer's first `len` bytes, to read into
    ///
    /// # Safety
    ///
    /// The returned bytes may be uninitialized: the caller must only write to them, and only read
    /// the bytes it has written.
    pub unsafe fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        let bytes = &mut self.0[..len];
        // SAFETY:
        // * bytes is non-null, thus the ptr used for creating the slice is non-null
        // * bytes is len bytes, thus slice is valid for all bytes
        // * buffer is aligned for a meta entry, thus so is the slice
        // * buffer is a single allocation, thus so is the slice
        // * bytes is not referenced while the slice is alive
        // * buffer is META_ENTRY_BUFFER_SIZE, which is less than isize::MAX, thus so is the slice
        unsafe { slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<u8>(), len) }
    }
}

/// An account parsed from its meta entry
pub enum AccountFromMetaEntry<'a> {
    /// the meta entry held all the account's data
    Complete {
        account: AccountWithoutData,
        data: &'a [u8],
    },
    /// the account's data must be read separately
    Partial(AccountDataRead),
}

impl AccountFromMetaEntry<'_> {
    pub fn passes_load_filter(
        &self,
        load_filter: impl Fn(u64, &Pubkey, usize) -> bool,
    ) -> bool {
        match self {
            Self::Complete { account, data } => {
                load_filter(account.lamports, &account.owner, data.len())
            }
            Self::Partial(data_read) => load_filter(
                data_read.account.lamports,
                &data_read.account.owner,
                data_read.data_len,
            ),
        }
    }
}

/// An account whose data must be read separately from its meta entry
pub struct AccountDataRead {
    pub address: Pubkey,
    pub account: AccountWithoutData,
    /// where to read the data from, in the format's file
    pub offset: FileSize,
    pub data_len: usize,
}

/// An account's fields from its meta entry, without its data
pub struct AccountWithoutData {
    pub lamports: u64,
    pub owner: Pubkey,
    pub executable: bool,
    pub rent_epoch: Epoch,
}

impl AccountWithoutData {
    /// The account, with its `data`
    pub fn with_data(self, data: Vec<u8>) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports: self.lamports,
            data,
            owner: self.owner,
            executable: self.executable,
            rent_epoch: self.rent_epoch,
        })
    }
}
