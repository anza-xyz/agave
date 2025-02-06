use {
    super::{scheduler_error::SchedulerError, transaction_state_container::StateContainer},
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
};

pub(crate) trait Scheduler<Tx: TransactionWithMeta> {
    /// Schedule transactions from `container`.
    /// pre-graph and pre-lock filters may be passed to be applied
    /// before specific actions internally.
    fn schedule<S: StateContainer<Tx>>(
        &mut self,
        container: &mut S,
        pre_graph_filter: impl Fn(&[&Tx], &mut [bool]),
        pre_lock_filter: impl Fn(&Tx) -> bool,
    ) -> Result<SchedulingSummary, SchedulerError>;

    /// Receive completed batches of transactions without blocking.
    /// Returns (num_transactions, num_retryable_transactions) on success.
    fn receive_completed(
        &mut self,
        container: &mut impl StateContainer<Tx>,
    ) -> Result<(usize, usize), SchedulerError>;
}

/// Metrics from scheduling transactions.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct SchedulingSummary {
    /// Number of transactions scheduled.
    pub num_scheduled: usize,
    /// Number of transactions that were not scheduled due to conflicts.
    pub num_unschedulable: usize,
    /// Number of transactions that were dropped due to filter.
    pub num_filtered_out: usize,
    /// Time spent filtering transactions
    pub filter_time_us: u64,
}
