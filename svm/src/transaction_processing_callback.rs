use solana_sdk::{account::AccountSharedData, clock::Slot, pubkey::Pubkey};

/// Runtime callbacks for transaction processing.
pub trait TransactionProcessingCallback {
    fn account_matches_owners(&self, account: &Pubkey, owners: &[Pubkey]) -> Option<usize>;

    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData>;

    // If `callback` returns `true`, the account will be stored into read cache after loading.
    // Otherwise, it will skip read cache.
    fn load_account_with(
        &self,
        pubkey: &Pubkey,
        callback: impl for<'a> Fn(&'a AccountSharedData) -> bool,
    ) -> Option<(AccountSharedData, Slot)>;

    fn add_builtin_account(&self, _name: &str, _program_id: &Pubkey) {}
}
