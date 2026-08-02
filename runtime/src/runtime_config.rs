use solana_compute_budget::compute_budget::ComputeBudget;

/// Encapsulates flags that can be used to tweak the runtime behavior.
#[derive(Debug, Default, Clone)]
pub struct RuntimeConfig {
    pub compute_budget: Option<ComputeBudget>,
    pub log_messages_bytes_limit: Option<usize>,
    pub transaction_account_lock_limit: Option<usize>,
    /// When true, skip storing transaction signature keys in the status cache.
    /// Message hash keys are still stored for duplicate transaction detection.
    pub skip_transaction_signatures_in_status_cache: bool,
}
