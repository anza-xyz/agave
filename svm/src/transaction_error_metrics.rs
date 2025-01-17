use std::num::Saturating;

#[derive(Debug, Default)]
pub struct TransactionErrorMetrics {
    pub total: Saturating<usize>,
    pub account_in_use: Saturating<usize>,
    pub too_many_account_locks: Saturating<usize>,
    pub account_loaded_twice: Saturating<usize>,
    pub account_not_found: Saturating<usize>,
    pub blockhash_not_found: Saturating<usize>,
    pub blockhash_too_old: Saturating<usize>,
    pub call_chain_too_deep: Saturating<usize>,
    pub already_processed: Saturating<usize>,
    pub instruction_error: Saturating<usize>,
    pub insufficient_funds: Saturating<usize>,
    pub invalid_account_for_fee: Saturating<usize>,
    pub invalid_account_index: Saturating<usize>,
    pub invalid_program_for_execution: Saturating<usize>,
    pub invalid_compute_budget: Saturating<usize>,
    pub not_allowed_during_cluster_maintenance: Saturating<usize>,
    pub invalid_writable_account: Saturating<usize>,
    pub invalid_rent_paying_account: Saturating<usize>,
    pub would_exceed_max_block_cost_limit: Saturating<usize>,
    pub would_exceed_max_account_cost_limit: Saturating<usize>,
    pub would_exceed_max_vote_cost_limit: Saturating<usize>,
    pub would_exceed_account_data_block_limit: Saturating<usize>,
    pub max_loaded_accounts_data_size_exceeded: Saturating<usize>,
    pub program_execution_temporarily_restricted: Saturating<usize>,
}

impl TransactionErrorMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accumulate(&mut self, other: &TransactionErrorMetrics) {
        self.total += other.total;
        self.account_in_use += other.account_in_use;
        self.too_many_account_locks += other.too_many_account_locks;
        self.account_loaded_twice += other.account_loaded_twice;
        self.account_not_found += other.account_not_found;
        self.blockhash_not_found += other.blockhash_not_found;
        self.blockhash_too_old += other.blockhash_too_old;
        self.call_chain_too_deep += other.call_chain_too_deep;
        self.already_processed += other.already_processed;
        self.instruction_error += other.instruction_error;
        self.insufficient_funds += other.insufficient_funds;
        self.invalid_account_for_fee += other.invalid_account_for_fee;
        self.invalid_account_index += other.invalid_account_index;
        self.invalid_program_for_execution += other.invalid_program_for_execution;
        self.invalid_compute_budget += other.invalid_compute_budget;
        self.not_allowed_during_cluster_maintenance += other.not_allowed_during_cluster_maintenance;
        self.invalid_writable_account += other.invalid_writable_account;
        self.invalid_rent_paying_account += other.invalid_rent_paying_account;
        self.would_exceed_max_block_cost_limit += other.would_exceed_max_block_cost_limit;
        self.would_exceed_max_account_cost_limit += other.would_exceed_max_account_cost_limit;
        self.would_exceed_max_vote_cost_limit += other.would_exceed_max_vote_cost_limit;
        self.would_exceed_account_data_block_limit += other.would_exceed_account_data_block_limit;
        self.max_loaded_accounts_data_size_exceeded += other.max_loaded_accounts_data_size_exceeded;
        self.program_execution_temporarily_restricted +=
            other.program_execution_temporarily_restricted;
    }
}
