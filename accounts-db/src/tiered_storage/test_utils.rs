#![cfg(test)]
//! Helper functions for TieredStorage tests
use {
    super::footer::TieredStorageFooter,
    crate::{
        account_storage::stored_account_info::StoredAccountInfo,
        tiered_storage::hot::RENT_EXEMPT_RENT_EPOCH,
    },
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
};

/// Create a test account based on the specified seed.
/// The created test account might have default rent_epoch
/// and write_version.
///
/// When the seed is zero, then a zero-lamport test account will be
/// created.
pub(super) fn create_test_account(seed: u64) -> (Pubkey, AccountSharedData) {
    let data_byte = seed as u8;
    let owner_byte = u8::MAX - data_byte;
    let account = Account {
        lamports: seed,
        data: std::iter::repeat_n(data_byte, seed as usize).collect(),
        // this will allow some test account sharing the same owner.
        owner: [owner_byte; 32].into(),
        executable: !seed.is_multiple_of(2),
        rent_epoch: if !seed.is_multiple_of(3) {
            seed
        } else {
            RENT_EXEMPT_RENT_EPOCH
        },
    };
    (Pubkey::new_unique(), AccountSharedData::from(account))
}

pub(super) fn verify_test_account(
    stored_account: &StoredAccountInfo<'_>,
    acc: &impl ReadableAccount,
    address: &Pubkey,
) {
    let (lamports, owner, data, executable) =
        (acc.lamports(), acc.owner(), acc.data(), acc.executable());

    assert_eq!(stored_account.lamports(), lamports);
    assert_eq!(stored_account.data().len(), data.len());
    assert_eq!(stored_account.data(), data);
    assert_eq!(stored_account.executable(), executable);
    assert_eq!(stored_account.owner(), owner);
    assert_eq!(stored_account.pubkey(), address);
}

pub(super) fn verify_test_account_with_footer(
    stored_account: &StoredAccountInfo<'_>,
    account: &impl ReadableAccount,
    address: &Pubkey,
    footer: &TieredStorageFooter,
) {
    verify_test_account(stored_account, account, address);
    assert!(footer.min_account_address <= *address);
    assert!(footer.max_account_address >= *address);
}
