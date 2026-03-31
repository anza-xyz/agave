use {
    solana_account::AccountSharedData, solana_pubkey::Pubkey, solana_runtime::bank::Bank,
    std::collections::HashMap,
};

pub(crate) fn get_account_from_overwrites_or_bank(
    pubkey: &Pubkey,
    bank: &Bank,
    overwrite_accounts: Option<&HashMap<Pubkey, AccountSharedData>>,
    rpc_read_only_accounts_cache_bypass: bool,
) -> Option<AccountSharedData> {
    overwrite_accounts
        .and_then(|accounts| accounts.get(pubkey).cloned())
        .or_else(|| {
            if rpc_read_only_accounts_cache_bypass {
                bank.get_account_with_fixed_root_no_cache(pubkey)
            } else {
                bank.get_account(pubkey)
            }
        })
}
