use {
    solana_account::{Account, ReadableAccount},
    solana_program_runtime::sysvar_cache::SysvarCache,
    solana_pubkey::Pubkey,
};

pub fn setup_sysvar_cache(input_accounts: &[(Pubkey, Account)]) -> SysvarCache {
    let mut sysvar_cache = SysvarCache::default();

    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if let Some(account) = input_accounts.iter().find(|(key, _)| key == pubkey) {
            if account.1.lamports() > 0 {
                callbackback(account.1.data());
            }
        }
    });

    sysvar_cache
}
