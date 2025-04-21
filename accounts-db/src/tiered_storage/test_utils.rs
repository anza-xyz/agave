#![cfg(test)]
//! Helper functions for TieredStorage tests
use {
    super::footer::TieredStorageFooter,
    crate::account_storage::{meta::StoredMeta, stored_account_info::StoredAccountInfo},
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
    solana_rent_collector::RENT_EXEMPT_RENT_EPOCH,
};

/// Create a test account based on the specified seed.
/// The created test account might have default rent_epoch
/// and write_version.
///
/// When the seed is zero, then a zero-lamport test account will be
/// created.
pub(super) fn create_test_account(seed: u64) -> (StoredMeta, AccountSharedData) {
    let data_byte = seed as u8;
    let owner_byte = u8::MAX - data_byte;
    let account = Account {
        lamports: seed,
        data: std::iter::repeat_n(data_byte, seed as usize).collect(),
        // this will allow some test account sharing the same owner.
        owner: [owner_byte; 32].into(),
        executable: seed % 2 > 0,
        rent_epoch: if seed % 3 > 0 {
            seed
        } else {
            RENT_EXEMPT_RENT_EPOCH
        },
    };

    let stored_meta = StoredMeta {
        write_version_obsolete: u64::MAX,
        pubkey: Pubkey::new_unique(),
        data_len: seed,
    };
    (stored_meta, AccountSharedData::from(account))
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
