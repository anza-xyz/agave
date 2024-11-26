//! Account loading callback, required by the migration API.

use {solana_account::AccountSharedData, solana_pubkey::Pubkey};

/// Account loading callback, required by the migration API.
pub trait AccountLoaderCallback {
    fn load_account(&self, pubkey: &Pubkey) -> Option<AccountSharedData>;
}
