use {
    solana_program_runtime::loaded_programs::ProgramCacheMatchCriteria,
    solana_sdk::{
        account::AccountSharedData, clock::Slot, feature_set::FeatureSet, hash::Hash,
        pubkey::Pubkey, rent_collector::RentCollector,
    },
    std::sync::Arc,
};

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

    fn get_last_blockhash_and_lamports_per_signature(&self) -> (Hash, u64);

    fn get_rent_collector(&self) -> &RentCollector;

    fn get_feature_set(&self) -> Arc<FeatureSet>;

    fn get_program_match_criteria(&self, _program: &Pubkey) -> ProgramCacheMatchCriteria {
        ProgramCacheMatchCriteria::NoCriteria
    }

    fn add_builtin_account(&self, _name: &str, _program_id: &Pubkey) {}
}
