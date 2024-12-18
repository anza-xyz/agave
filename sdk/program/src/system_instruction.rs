#[deprecated(since = "2.2.0", note = "Use `solana_system_interface` crate instead")]
pub use solana_system_interface::{
    error::SystemError,
    instruction::{
        advance_nonce_account, allocate, allocate_with_seed, assign, assign_with_seed,
        authorize_nonce_account, create_account, create_account_with_seed, create_nonce_account,
        create_nonce_account_with_seed, transfer, transfer_many, transfer_with_seed,
        upgrade_nonce_account, withdraw_nonce_account, SystemInstruction,
    },
    MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION, MAX_PERMITTED_DATA_LENGTH,
};
