use {
    solana_poh::transaction_recorder::RecordTransactionsTimings, solana_svm_timings::ExecuteTimings,
};

#[derive(Default, Debug)]
pub struct LeaderExecuteAndCommitTimings {
    pub load_execute_us: u64,
    pub freeze_lock_us: u64,
    pub record_us: u64,
    pub commit_us: u64,
    pub post_commit_us: u64,
    pub record_transactions_timings: RecordTransactionsTimings,
    pub execute_timings: ExecuteTimings,
}
