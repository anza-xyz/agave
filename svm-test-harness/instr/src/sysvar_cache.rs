use {
    solana_account::{Account, ReadableAccount},
    solana_program_runtime::sysvar_cache::SysvarCache,
    solana_pubkey::Pubkey,
};

/// Populate a `SysvarCache` via `fill_missing_entries` from any sysvar accounts.
pub fn fill_from_accounts(sysvar_cache: &mut SysvarCache, accounts: &[(Pubkey, Account)]) {
    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if let Some(account) = accounts.iter().find(|(key, _)| key == pubkey) {
            if account.1.lamports() > 0 {
                callbackback(account.1.data());
            }
        }
    });
}
