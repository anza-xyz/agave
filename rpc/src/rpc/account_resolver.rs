use {
    solana_account::AccountSharedData, solana_accounts_db::accounts_db::PopulateReadCache,
    solana_pubkey::Pubkey, solana_runtime::bank::Bank, std::collections::HashMap,
};

pub(crate) fn get_account_from_overwrites_or_bank(
    pubkey: &Pubkey,
    bank: &Bank,
    overwrite_accounts: Option<&HashMap<Pubkey, AccountSharedData>>,
    rpc_populate_read_only_accounts_cache: bool,
) -> Option<AccountSharedData> {
    overwrite_accounts
        .and_then(|accounts| accounts.get(pubkey).cloned())
        .or_else(|| {
            let populate = if rpc_populate_read_only_accounts_cache {
                PopulateReadCache::True
            } else {
                PopulateReadCache::False
            };
            bank.get_account_with_cache_option(pubkey, populate)
        })
}
