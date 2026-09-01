//! Helpers for AppendVec tests and benches
#![cfg(feature = "dev-context-only-utils")]
use {solana_account::AccountSharedData, solana_pubkey::Pubkey};

/// return a test account.
/// Note that `sample`=0 returns a fully default account with a default pubkey.
pub fn create_test_account(sample: usize) -> (Pubkey, AccountSharedData) {
    let data_len = sample % 256;
    let mut account = AccountSharedData::new(sample as u64, 0, &Pubkey::default());
    account.set_data_from_slice(&vec![data_len as u8; data_len]);
    (Pubkey::default(), account)
}

/// Create a test account for the given `data_len`.
/// This is useful to create very large test account.
pub fn create_test_account_with(data_len: usize) -> (Pubkey, AccountSharedData) {
    let account = AccountSharedData::new(100, data_len, &Pubkey::default());
    (Pubkey::default(), account)
}
