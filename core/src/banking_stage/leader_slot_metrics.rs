use {
    super::{
        consumer::LeaderProcessedTransactionCounts,
        leader_slot_timing_metrics::{LeaderExecuteAndCommitTimings, LeaderSlotTimingMetrics},
        vote_storage::VoteBatchInsertionMetrics,
    },
    crate::banking_stage::vote_packet_receiver::PacketReceiverStats,
    solana_clock::Slot,
    solana_runtime::bank::Bank,
    solana_svm::transaction_error_metrics::*,
    std::{num::Saturating, sync::Arc},
};

/// A summary of what happened to transactions passed to the processing pipeline.
/// Transactions can
/// 1) Did not even make it to processing due to being filtered out by things like AccountInUse
///    lock conflicts or CostModel compute limits. These types of errors are retryable and
///    counted in `Self::retryable_transaction_indexes`.
/// 2) Did not process due to some fatal error like too old, or duplicate signature. These
///    will be dropped from the transactions queue and not counted in `Self::retryable_transaction_indexes`
/// 3) Were processed and committed, captured by `transaction_counts` below.
/// 4) Were processed and failed commit, captured by `transaction_counts` below.
pub(crate) struct ProcessTransactionsSummary {
    /// Returns true if we hit the end of the block/max PoH height for the block
    /// before processing all the transactions in the batch.
    pub reached_max_poh_height: bool,

    /// Total transaction counts tracked for reporting `LeaderSlotMetrics`. See
    /// description of struct above for possible outcomes for these transactions
    pub transaction_counts: CommittedTransactionsCounts,

    /// Indexes of transactions in the transactions slice that were not
    /// committed but are retryable
    pub retryable_transaction_indexes: Vec<usize>,

    /// The number of transactions filtered out by the cost model
    pub cost_model_throttled_transactions_count: u64,

    /// Total amount of time spent running the cost model
    pub cost_model_us: u64,

    /// Breakdown of time spent executing and committing transactions
    pub execute_and_commit_timings: LeaderExecuteAndCommitTimings,

    /// Breakdown of all the transaction errors from transactions passed for
    /// execution
    pub error_counters: TransactionErrorMetrics,
}

#[derive(Debug, Default, PartialEq)]
pub struct CommittedTransactionsCounts {
    /// Total number of transactions that were passed as candidates for processing
    pub attempted_processing_count: Saturating<u64>,
    /// Total number of transactions that made it into the block
    pub committed_transactions_count: Saturating<u64>,
    /// Total number of transactions that made it into the block where the transactions
    /// output from processing was success/no error.
    pub committed_transactions_with_successful_result_count: Saturating<u64>,
    /// All transactions that were processed but then failed record because the
    /// slot ended
    pub processed_but_failed_commit: Saturating<u64>,
}

impl CommittedTransactionsCounts {
    pub fn accumulate(
        &mut self,
        transaction_counts: &LeaderProcessedTransactionCounts,
        committed: bool,
    ) {
        self.attempted_processing_count += transaction_counts.attempted_processing_count;
        if committed {
            self.committed_transactions_count += transaction_counts.processed_count;
            self.committed_transactions_with_successful_result_count +=
                transaction_counts.processed_with_successful_result_count;
        } else {
            self.processed_but_failed_commit += transaction_counts.processed_count;
        }
    }
}

// Metrics describing packets ingested/processed in various parts of BankingStage during this
// validator's leader slot
#[derive(Debug, Default)]
struct LeaderSlotPacketCountMetrics {
    // total number of live packets TPU received from verified receiver for processing.
    total_new_valid_packets: u64,

    // total number of packets TPU received from sigverify that failed signature verification.
    newly_failed_sigverify_count: u64,

    // total number of packets filtered due to sanitization failures during receiving from sigverify
    failed_sanitization_count: u64,

    // total number of packets filtered due to prioritization failures during receiving from sigverify
    failed_prioritization_count: u64,

    // total number of invalid vote packets filtered out during receiving from sigverify
    invalid_votes_count: u64,

    // total number of dropped packet due to the thread's buffered packets capacity being reached.
    exceeded_buffer_limit_dropped_packets_count: u64,

    // total number of packets that got added to the pending buffer after arriving to BankingStage
    newly_buffered_packets_count: u64,

    // total number of transactions in the buffer that were filtered out due to things like age and
    // duplicate signature checks
    retryable_packets_filtered_count: u64,

    // total number of transactions that attempted processing in this slot. Should equal the sum
    // of `committed_transactions_count`, `retryable_errored_transaction_count`, and
    // `nonretryable_errored_transactions_count`.
    transactions_attempted_processing_count: Saturating<u64>,

    // total number of transactions that were executed and committed into the block
    // on this thread
    committed_transactions_count: Saturating<u64>,

    // total number of transactions that were executed, got a successful execution output/no error,
    // and were then committed into the block
    committed_transactions_with_successful_result_count: Saturating<u64>,

    // total number of transactions that were not executed or failed commit, BUT were added back to the buffered
    // queue because they were retryable errors
    retryable_errored_transaction_count: Saturating<u64>,

    // The size of the unprocessed buffer at the end of the slot
    end_of_slot_unprocessed_buffer_len: u64,

    // total number of transactions that were rebuffered into the queue after not being
    // executed on a previous pass
    retryable_packets_count: u64,

    // total number of transactions that attempted execution due to some fatal error (too old, duplicate signature, etc.)
    // AND were dropped from the buffered queue
    nonretryable_errored_transactions_count: Saturating<u64>,

    // total number of transactions that were executed, but failed to be committed into the Poh stream because
    // the block ended. Some of these may be already counted in `nonretryable_errored_transactions_count` if they
    // then hit the age limit after failing to be committed.
    executed_transactions_failed_commit_count: Saturating<u64>,

    // total number of transactions that were excluded from the block because there were concurrent write locks active.
    // These transactions are added back to the buffered queue and are already counted in
    // `self.retrayble_errored_transaction_count`.
    account_lock_throttled_transactions_count: Saturating<u64>,

    // total number of transactions that were excluded from the block because their write
    // account locks exceed the limit.
    // These transactions are not retried.
    account_locks_limit_throttled_transactions_count: Saturating<u64>,

    // total number of transactions that were excluded from the block because they were too expensive
    // according to the cost model. These transactions are added back to the buffered queue and are
    // already counted in `self.retrayble_errored_transaction_count`.
    cost_model_throttled_transactions_count: Saturating<u64>,
}

impl LeaderSlotPacketCountMetrics {
    fn new() -> Self {
        Self::default()
    }

    fn report(&self, slot: Slot) {
        let &Self {
            total_new_valid_packets,
            newly_failed_sigverify_count,
            failed_sanitization_count,
            failed_prioritization_count,
            invalid_votes_count,
            exceeded_buffer_limit_dropped_packets_count,
            newly_buffered_packets_count,
            retryable_packets_filtered_count,
            transactions_attempted_processing_count:
                Saturating(transactions_attempted_processing_count),
            committed_transactions_count: Saturating(committed_transactions_count),
            committed_transactions_with_successful_result_count:
                Saturating(committed_transactions_with_successful_result_count),
            retryable_errored_transaction_count: Saturating(retryable_errored_transaction_count),
            retryable_packets_count,
            nonretryable_errored_transactions_count:
                Saturating(nonretryable_errored_transactions_count),
            executed_transactions_failed_commit_count:
                Saturating(executed_transactions_failed_commit_count),
            account_lock_throttled_transactions_count:
                Saturating(account_lock_throttled_transactions_count),
            account_locks_limit_throttled_transactions_count:
                Saturating(account_locks_limit_throttled_transactions_count),
            cost_model_throttled_transactions_count:
                Saturating(cost_model_throttled_transactions_count),
            end_of_slot_unprocessed_buffer_len,
        } = self;
        datapoint_info!(
            "banking_stage-vote_slot_packet_counts",
            ("slot", slot, i64),
            ("total_new_valid_packets", total_new_valid_packets, i64),
            (
                "newly_failed_sigverify_count",
                newly_failed_sigverify_count,
                i64
            ),
            ("failed_sanitization_count", failed_sanitization_count, i64),
            (
                "failed_prioritization_count",
                failed_prioritization_count,
                i64
            ),
            ("invalid_votes_count", invalid_votes_count, i64),
            (
                "exceeded_buffer_limit_dropped_packets_count",
                exceeded_buffer_limit_dropped_packets_count,
                i64
            ),
            (
                "newly_buffered_packets_count",
                newly_buffered_packets_count,
                i64
            ),
            (
                "retryable_packets_filtered_count",
                retryable_packets_filtered_count,
                i64
            ),
            (
                "transactions_attempted_processing_count",
                transactions_attempted_processing_count,
                i64
            ),
            (
                "committed_transactions_count",
                committed_transactions_count,
                i64
            ),
            (
                "committed_transactions_with_successful_result_count",
                committed_transactions_with_successful_result_count,
                i64
            ),
            (
                "retryable_errored_transaction_count",
                retryable_errored_transaction_count,
                i64
            ),
            ("retryable_packets_count", retryable_packets_count, i64),
            (
                "nonretryable_errored_transactions_count",
                nonretryable_errored_transactions_count,
                i64
            ),
            (
                "executed_transactions_failed_commit_count",
                executed_transactions_failed_commit_count,
                i64
            ),
            (
                "account_lock_throttled_transactions_count",
                account_lock_throttled_transactions_count,
                i64
            ),
            (
                "account_locks_limit_throttled_transactions_count",
                account_locks_limit_throttled_transactions_count,
                i64
            ),
            (
                "cost_model_throttled_transactions_count",
                cost_model_throttled_transactions_count,
                i64
            ),
            (
                "end_of_slot_unprocessed_buffer_len",
                end_of_slot_unprocessed_buffer_len,
                i64
            ),
        );
    }
}

fn report_transaction_error_metrics(errors: &TransactionErrorMetrics, slot: Slot) {
    datapoint_info!(
        "banking_stage-vote_slot_transaction_errors",
        ("slot", slot as i64, i64),
        ("total", errors.total.0 as i64, i64),
        ("account_in_use", errors.account_in_use.0 as i64, i64),
        (
            "too_many_account_locks",
            errors.too_many_account_locks.0 as i64,
            i64
        ),
        (
            "account_loaded_twice",
            errors.account_loaded_twice.0 as i64,
            i64
        ),
        ("account_not_found", errors.account_not_found.0 as i64, i64),
        (
            "blockhash_not_found",
            errors.blockhash_not_found.0 as i64,
            i64
        ),
        ("blockhash_too_old", errors.blockhash_too_old.0 as i64, i64),
        (
            "call_chain_too_deep",
            errors.call_chain_too_deep.0 as i64,
            i64
        ),
        ("already_processed", errors.already_processed.0 as i64, i64),
        ("instruction_error", errors.instruction_error.0 as i64, i64),
        (
            "insufficient_funds",
            errors.insufficient_funds.0 as i64,
            i64
        ),
        (
            "invalid_account_for_fee",
            errors.invalid_account_for_fee.0 as i64,
            i64
        ),
        (
            "invalid_account_index",
            errors.invalid_account_index.0 as i64,
            i64
        ),
        (
            "invalid_program_for_execution",
            errors.invalid_program_for_execution.0 as i64,
            i64
        ),
        (
            "invalid_compute_budget",
            errors.invalid_compute_budget.0 as i64,
            i64
        ),
        (
            "not_allowed_during_cluster_maintenance",
            errors.not_allowed_during_cluster_maintenance.0 as i64,
            i64
        ),
        (
            "invalid_writable_account",
            errors.invalid_writable_account.0 as i64,
            i64
        ),
        (
            "invalid_rent_paying_account",
            errors.invalid_rent_paying_account.0 as i64,
            i64
        ),
        (
            "would_exceed_max_block_cost_limit",
            errors.would_exceed_max_block_cost_limit.0 as i64,
            i64
        ),
        (
            "would_exceed_max_account_cost_limit",
            errors.would_exceed_max_account_cost_limit.0 as i64,
            i64
        ),
        (
            "would_exceed_max_vote_cost_limit",
            errors.would_exceed_max_vote_cost_limit.0 as i64,
            i64
        ),
        (
            "would_exceed_account_data_block_limit",
            errors.would_exceed_account_data_block_limit.0 as i64,
            i64
        ),
        (
            "max_loaded_accounts_data_size_exceeded",
            errors.max_loaded_accounts_data_size_exceeded.0 as i64,
            i64
        ),
        (
            "program_execution_temporarily_restricted",
            errors.program_execution_temporarily_restricted.0 as i64,
            i64
        ),
    );
}

#[derive(Debug)]
pub(crate) struct LeaderSlotMetrics {
    // aggregate metrics per slot
    slot: Slot,

    packet_count_metrics: LeaderSlotPacketCountMetrics,

    transaction_error_metrics: TransactionErrorMetrics,

    vote_packet_count_metrics: VotePacketCountMetrics,

    timing_metrics: LeaderSlotTimingMetrics,

    // Used by tests to check if the `self.report()` method was called
    is_reported: bool,
}

impl LeaderSlotMetrics {
    pub(crate) fn new(slot: Slot) -> Self {
        Self {
            slot,
            packet_count_metrics: LeaderSlotPacketCountMetrics::new(),
            transaction_error_metrics: TransactionErrorMetrics::new(),
            vote_packet_count_metrics: VotePacketCountMetrics::new(),
            timing_metrics: LeaderSlotTimingMetrics::new(),
            is_reported: false,
        }
    }

    pub(crate) fn report(&mut self) {
        self.is_reported = true;

        self.timing_metrics.report(self.slot);
        report_transaction_error_metrics(&self.transaction_error_metrics, self.slot);
        self.packet_count_metrics.report(self.slot);
        self.vote_packet_count_metrics.report(self.slot);
    }

    /// Returns `Some(self.slot)` if the metrics have been reported, otherwise returns None
    fn reported_slot(&self) -> Option<Slot> {
        if self.is_reported {
            Some(self.slot)
        } else {
            None
        }
    }

    fn mark_slot_end_detected(&mut self) {
        self.timing_metrics.mark_slot_end_detected();
    }
}

// Metrics describing vote tx packets that were processed in the tpu vote thread as well as
// extraneous votes that were filtered out
#[derive(Debug, Default)]
pub(crate) struct VotePacketCountMetrics {
    // How many votes ingested from gossip were dropped
    dropped_gossip_votes: u64,

    // How many votes ingested from tpu were dropped
    dropped_tpu_votes: u64,
}

impl VotePacketCountMetrics {
    fn new() -> Self {
        Self::default()
    }

    fn report(&self, slot: Slot) {
        datapoint_info!(
            "banking_stage-vote_packet_counts",
            ("slot", slot, i64),
            ("dropped_gossip_votes", self.dropped_gossip_votes, i64),
            ("dropped_tpu_votes", self.dropped_tpu_votes, i64)
        );
    }
}

#[derive(Debug)]
pub(crate) enum MetricsTrackerAction {
    Noop,
    ReportAndResetTracker,
    NewTracker(Option<LeaderSlotMetrics>),
    ReportAndNewTracker(Option<LeaderSlotMetrics>),
}

#[derive(Debug, Default)]
pub struct LeaderSlotMetricsTracker {
    // Only `Some` if BankingStage detects it's time to construct our leader slot,
    // otherwise `None`
    leader_slot_metrics: Option<LeaderSlotMetrics>,
}

impl LeaderSlotMetricsTracker {
    // Check leader slot, return MetricsTrackerAction to be applied by apply_action()
    pub(crate) fn check_leader_slot_boundary(
        &mut self,
        bank: Option<&Arc<Bank>>,
    ) -> MetricsTrackerAction {
        match (self.leader_slot_metrics.as_mut(), bank) {
            (None, None) => MetricsTrackerAction::Noop,

            (Some(leader_slot_metrics), None) => {
                leader_slot_metrics.mark_slot_end_detected();
                MetricsTrackerAction::ReportAndResetTracker
            }

            // Our leader slot has begun, time to create a new slot tracker
            (None, Some(bank)) => {
                MetricsTrackerAction::NewTracker(Some(LeaderSlotMetrics::new(bank.slot())))
            }

            (Some(leader_slot_metrics), Some(bank)) => {
                if leader_slot_metrics.slot != bank.slot() {
                    // Last slot has ended, new slot has began
                    leader_slot_metrics.mark_slot_end_detected();
                    MetricsTrackerAction::ReportAndNewTracker(Some(LeaderSlotMetrics::new(
                        bank.slot(),
                    )))
                } else {
                    MetricsTrackerAction::Noop
                }
            }
        }
    }

    pub(crate) fn apply_action(&mut self, action: MetricsTrackerAction) -> Option<Slot> {
        match action {
            MetricsTrackerAction::Noop => None,
            MetricsTrackerAction::ReportAndResetTracker => {
                let mut reported_slot = None;
                if let Some(leader_slot_metrics) = self.leader_slot_metrics.as_mut() {
                    leader_slot_metrics.report();
                    reported_slot = leader_slot_metrics.reported_slot();
                }
                self.leader_slot_metrics = None;
                reported_slot
            }
            MetricsTrackerAction::NewTracker(new_slot_metrics) => {
                self.leader_slot_metrics = new_slot_metrics;
                self.leader_slot_metrics.as_ref().unwrap().reported_slot()
            }
            MetricsTrackerAction::ReportAndNewTracker(new_slot_metrics) => {
                let mut reported_slot = None;
                if let Some(leader_slot_metrics) = self.leader_slot_metrics.as_mut() {
                    leader_slot_metrics.report();
                    reported_slot = leader_slot_metrics.reported_slot();
                }
                self.leader_slot_metrics = new_slot_metrics;
                reported_slot
            }
        }
    }

    pub(crate) fn accumulate_process_transactions_summary(
        &mut self,
        process_transactions_summary: &ProcessTransactionsSummary,
    ) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            let &ProcessTransactionsSummary {
                transaction_counts:
                    CommittedTransactionsCounts {
                        attempted_processing_count: Saturating(attempted_processing_count),
                        committed_transactions_count: Saturating(committed_transactions_count),
                        committed_transactions_with_successful_result_count:
                            Saturating(committed_transactions_with_successful_result_count),
                        processed_but_failed_commit: Saturating(processed_but_failed_commit),
                    },
                ref retryable_transaction_indexes,
                cost_model_throttled_transactions_count,
                cost_model_us,
                ref execute_and_commit_timings,
                ref error_counters,
                ..
            } = process_transactions_summary;

            leader_slot_metrics
                .packet_count_metrics
                .transactions_attempted_processing_count += attempted_processing_count;

            leader_slot_metrics
                .packet_count_metrics
                .committed_transactions_count += committed_transactions_count;

            leader_slot_metrics
                .packet_count_metrics
                .committed_transactions_with_successful_result_count +=
                committed_transactions_with_successful_result_count;

            leader_slot_metrics
                .packet_count_metrics
                .executed_transactions_failed_commit_count += processed_but_failed_commit;

            leader_slot_metrics
                .packet_count_metrics
                .retryable_errored_transaction_count += retryable_transaction_indexes.len() as u64;

            leader_slot_metrics
                .packet_count_metrics
                .nonretryable_errored_transactions_count += attempted_processing_count
                .saturating_sub(committed_transactions_count)
                .saturating_sub(retryable_transaction_indexes.len() as u64);

            leader_slot_metrics
                .packet_count_metrics
                .account_lock_throttled_transactions_count +=
                error_counters.account_in_use.0 as u64;

            leader_slot_metrics
                .packet_count_metrics
                .account_locks_limit_throttled_transactions_count +=
                error_counters.too_many_account_locks.0 as u64;

            leader_slot_metrics
                .packet_count_metrics
                .cost_model_throttled_transactions_count += cost_model_throttled_transactions_count;

            leader_slot_metrics
                .packet_count_metrics
                .cost_model_throttled_transactions_count += cost_model_throttled_transactions_count;

            leader_slot_metrics
                .timing_metrics
                .process_packets_timings
                .cost_model_us += cost_model_us;

            leader_slot_metrics
                .timing_metrics
                .execute_and_commit_timings
                .accumulate(execute_and_commit_timings);
        }
    }

    pub(crate) fn accumulate_vote_batch_insertion_metrics(
        &mut self,
        vote_batch_insertion_metrics: &VoteBatchInsertionMetrics,
    ) {
        self.increment_exceeded_buffer_limit_dropped_packets_count(
            vote_batch_insertion_metrics.total_dropped_packets() as u64,
        );
        self.increment_dropped_gossip_vote_count(
            vote_batch_insertion_metrics.dropped_gossip_packets() as u64,
        );
        self.increment_dropped_tpu_vote_count(
            vote_batch_insertion_metrics.dropped_tpu_packets() as u64
        );
    }

    pub(crate) fn accumulate_transaction_errors(
        &mut self,
        error_metrics: &TransactionErrorMetrics,
    ) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .transaction_error_metrics
                .accumulate(error_metrics);
        }
    }

    // Packet inflow/outflow/processing metrics
    pub(crate) fn increment_received_packet_counts(&mut self, stats: PacketReceiverStats) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            let metrics = &mut leader_slot_metrics.packet_count_metrics;
            let PacketReceiverStats {
                passed_sigverify_count: Saturating(passed_sigverify_count),
                failed_sigverify_count: Saturating(failed_sigverify_count),
                invalid_vote_count: Saturating(invalid_vote_count),
                failed_prioritization_count: Saturating(failed_prioritization_count),
                failed_sanitization_count: Saturating(failed_sanitization_count),
            } = stats;

            metrics.total_new_valid_packets += passed_sigverify_count;
            metrics.newly_failed_sigverify_count += failed_sigverify_count;
            metrics.invalid_votes_count += invalid_vote_count;
            metrics.failed_prioritization_count += failed_prioritization_count;
            metrics.failed_sanitization_count += failed_sanitization_count;
        }
    }

    pub(crate) fn increment_exceeded_buffer_limit_dropped_packets_count(&mut self, count: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .packet_count_metrics
                .exceeded_buffer_limit_dropped_packets_count += count;
        }
    }

    pub(crate) fn increment_newly_buffered_packets_count(&mut self, count: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .packet_count_metrics
                .newly_buffered_packets_count += count;
        }
    }

    pub(crate) fn increment_retryable_packets_filtered_count(&mut self, count: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .packet_count_metrics
                .retryable_packets_filtered_count += count;
        }
    }

    pub(crate) fn increment_retryable_packets_count(&mut self, count: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .packet_count_metrics
                .retryable_packets_count += count;
        }
    }

    pub(crate) fn set_end_of_slot_unprocessed_buffer_len(&mut self, len: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .packet_count_metrics
                .end_of_slot_unprocessed_buffer_len = len;
        }
    }

    // Outermost banking thread's loop timing metrics
    pub(crate) fn increment_process_buffered_packets_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .outer_loop_timings
                .process_buffered_packets_us += us;
        }
    }

    pub(crate) fn increment_receive_and_buffer_packets_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .outer_loop_timings
                .receive_and_buffer_packets_us += us;

            leader_slot_metrics
                .timing_metrics
                .outer_loop_timings
                .receive_and_buffer_packets_invoked_count += 1;
        }
    }

    // Processing buffer timing metrics
    pub(crate) fn increment_make_decision_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .process_buffered_packets_timings
                .make_decision_us += us;
        }
    }

    pub(crate) fn increment_consume_buffered_packets_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .process_buffered_packets_timings
                .consume_buffered_packets_us += us;
        }
    }

    pub(crate) fn increment_process_packets_transactions_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .consume_buffered_packets_timings
                .process_packets_transactions_us += us
        }
    }

    pub(crate) fn increment_process_transactions_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .process_packets_timings
                .process_transactions_us += us;
        }
    }

    pub(crate) fn increment_filter_retryable_packets_us(&mut self, us: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .timing_metrics
                .process_packets_timings
                .filter_retryable_packets_us += us;
        }
    }

    pub(crate) fn increment_dropped_gossip_vote_count(&mut self, count: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .vote_packet_count_metrics
                .dropped_gossip_votes += count;
        }
    }

    pub(crate) fn increment_dropped_tpu_vote_count(&mut self, count: u64) {
        if let Some(leader_slot_metrics) = &mut self.leader_slot_metrics {
            leader_slot_metrics
                .vote_packet_count_metrics
                .dropped_tpu_votes += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_pubkey::Pubkey,
        solana_runtime::{bank::Bank, genesis_utils::create_genesis_config},
        std::{mem, sync::Arc},
    };

    struct TestSlotBoundaryComponents {
        first_bank: Arc<Bank>,
        next_bank: Arc<Bank>,
        leader_slot_metrics_tracker: LeaderSlotMetricsTracker,
    }

    fn setup_test_slot_boundary_banks() -> TestSlotBoundaryComponents {
        let genesis = create_genesis_config(10);
        let first_bank = Arc::new(Bank::new_for_tests(&genesis.genesis_config));

        // Create a child descended from the first bank
        let next_bank = Arc::new(Bank::new_from_parent(
            first_bank.clone(),
            &Pubkey::new_unique(),
            first_bank.slot() + 1,
        ));

        let leader_slot_metrics_tracker = LeaderSlotMetricsTracker::default();

        TestSlotBoundaryComponents {
            first_bank,
            next_bank,
            leader_slot_metrics_tracker,
        }
    }

    #[test]
    pub fn test_update_on_leader_slot_boundary_not_leader_to_not_leader() {
        let TestSlotBoundaryComponents {
            mut leader_slot_metrics_tracker,
            ..
        } = setup_test_slot_boundary_banks();
        // Test that with no bank being tracked, and no new bank being tracked, nothing is reported
        let action = leader_slot_metrics_tracker.check_leader_slot_boundary(None);
        assert_eq!(
            mem::discriminant(&MetricsTrackerAction::Noop),
            mem::discriminant(&action)
        );
        assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_none());
    }

    #[test]
    pub fn test_update_on_leader_slot_boundary_not_leader_to_leader() {
        let TestSlotBoundaryComponents {
            first_bank,
            mut leader_slot_metrics_tracker,
            ..
        } = setup_test_slot_boundary_banks();

        // Test case where the thread has not detected a leader bank, and now sees a leader bank.
        // Metrics should not be reported because leader slot has not ended
        assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_none());
        let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&first_bank));
        assert_eq!(
            mem::discriminant(&MetricsTrackerAction::NewTracker(None)),
            mem::discriminant(&action)
        );
        assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_some());
    }

    #[test]
    pub fn test_update_on_leader_slot_boundary_leader_to_not_leader() {
        let TestSlotBoundaryComponents {
            first_bank,
            mut leader_slot_metrics_tracker,
            ..
        } = setup_test_slot_boundary_banks();

        // Test case where the thread has a leader bank, and now detects there's no more leader bank,
        // implying the slot has ended. Metrics should be reported for `first_bank.slot()`,
        // because that leader slot has just ended.
        {
            // Setup first_bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&first_bank));
            assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        }
        {
            // Assert reporting if slot has ended
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(None);
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::ReportAndResetTracker),
                mem::discriminant(&action)
            );
            assert_eq!(
                leader_slot_metrics_tracker.apply_action(action).unwrap(),
                first_bank.slot()
            );
            assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_none());
        }
        {
            // Assert no-op if still no new bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(None);
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::Noop),
                mem::discriminant(&action)
            );
        }
    }

    #[test]
    pub fn test_update_on_leader_slot_boundary_leader_to_leader_same_slot() {
        let TestSlotBoundaryComponents {
            first_bank,
            mut leader_slot_metrics_tracker,
            ..
        } = setup_test_slot_boundary_banks();

        // Test case where the thread has a leader bank, and now detects the same leader bank,
        // implying the slot is still running. Metrics should not be reported
        {
            // Setup with first_bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&first_bank));
            assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        }
        {
            // Assert nop-op if same bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&first_bank));
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::Noop),
                mem::discriminant(&action)
            );
            assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        }
        {
            // Assert reporting if slot has ended
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(None);
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::ReportAndResetTracker),
                mem::discriminant(&action)
            );
            assert_eq!(
                leader_slot_metrics_tracker.apply_action(action).unwrap(),
                first_bank.slot()
            );
            assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_none());
        }
    }

    #[test]
    pub fn test_update_on_leader_slot_boundary_leader_to_leader_bigger_slot() {
        let TestSlotBoundaryComponents {
            first_bank,
            next_bank,
            mut leader_slot_metrics_tracker,
        } = setup_test_slot_boundary_banks();

        // Test case where the thread has a leader bank, and now detects there's a new leader bank
        // for a bigger slot, implying the slot has ended. Metrics should be reported for the
        // smaller slot
        {
            // Setup with first_bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&first_bank));
            assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        }
        {
            // Assert reporting if new bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&next_bank));
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::ReportAndNewTracker(None)),
                mem::discriminant(&action)
            );
            assert_eq!(
                leader_slot_metrics_tracker.apply_action(action).unwrap(),
                first_bank.slot()
            );
            assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_some());
        }
        {
            // Assert reporting if slot has ended
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(None);
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::ReportAndResetTracker),
                mem::discriminant(&action)
            );
            assert_eq!(
                leader_slot_metrics_tracker.apply_action(action).unwrap(),
                next_bank.slot()
            );
            assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_none());
        }
    }

    #[test]
    pub fn test_update_on_leader_slot_boundary_leader_to_leader_smaller_slot() {
        let TestSlotBoundaryComponents {
            first_bank,
            next_bank,
            mut leader_slot_metrics_tracker,
        } = setup_test_slot_boundary_banks();
        // Test case where the thread has a leader bank, and now detects there's a new leader bank
        // for a smaller slot, implying the slot has ended. Metrics should be reported for the
        // bigger slot
        {
            // Setup with next_bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&next_bank));
            assert!(leader_slot_metrics_tracker.apply_action(action).is_none());
        }
        {
            // Assert reporting if new bank
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(Some(&first_bank));
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::ReportAndNewTracker(None)),
                mem::discriminant(&action)
            );
            assert_eq!(
                leader_slot_metrics_tracker.apply_action(action).unwrap(),
                next_bank.slot()
            );
            assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_some());
        }
        {
            // Assert reporting if slot has ended
            let action = leader_slot_metrics_tracker.check_leader_slot_boundary(None);
            assert_eq!(
                mem::discriminant(&MetricsTrackerAction::ReportAndResetTracker),
                mem::discriminant(&action)
            );
            assert_eq!(
                leader_slot_metrics_tracker.apply_action(action).unwrap(),
                first_bank.slot()
            );
            assert!(leader_slot_metrics_tracker.leader_slot_metrics.is_none());
        }
    }
}
