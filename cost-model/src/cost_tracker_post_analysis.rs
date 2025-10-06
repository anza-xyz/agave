use {solana_pubkey::Pubkey, std::collections::HashMap};

/// Trait to help with post-analysis of a given block
pub trait CostTrackerPostAnalysis {
    /// Only use in post-analyze to avoid lock contention
    /// Do not use in the hot path
    fn get_cost_by_writable_accounts(&self) -> &HashMap<Pubkey, u64, ahash::RandomState>;
}
