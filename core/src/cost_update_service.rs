//! this service asynchronously reports CostTracker stats

use {
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_cost_model::cost_tracker::CostTrackerStats,
    solana_metrics::datapoint_info,
    solana_runtime::bank::Bank,
    std::{
        sync::Arc,
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};
pub enum CostUpdate {
    FrozenBank {
        bank: Arc<Bank>,
        is_leader_block: bool,
    },
}

pub type CostUpdateReceiver = Receiver<CostUpdate>;

pub struct CostUpdateService {
    thread_hdl: JoinHandle<()>,
}

// The maximum number of retries to check if CostTracker::in_flight_transaction_count() has settled
// to zero. Bail out after this many retries; the in-flight count is reported so this is ok
const MAX_LOOP_COUNT: usize = 25;
// Throttle checking the count to avoid excessive polling
const LOOP_LIMITER: Duration = Duration::from_millis(10);

pub fn report_cost_tracker_stats(
    stats: &CostTrackerStats,
    bank_slot: Slot,
    is_leader: bool,
    total_transaction_fee: u64,
    total_priority_fee: u64,
) {
    // Skip reporting if the block is empty.
    if stats.transaction_count == 0 {
        return;
    }

    datapoint_info!(
        "cost_tracker_stats",
        "is_leader" => is_leader.to_string(),
        ("bank_slot", bank_slot, i64),
        ("block_cost", stats.block_cost, i64),
        ("transaction_count", stats.transaction_count, i64),
        ("number_of_accounts", stats.number_of_accounts, i64),
        ("costliest_account", stats.costliest_account.to_string(), String),
        ("costliest_account_cost", stats.costliest_account_cost, i64),
        (
            "allocated_accounts_data_size",
            stats.allocated_accounts_data_size,
            i64
        ),
        (
            "transaction_signature_count",
            stats.transaction_signature_count,
            i64
        ),
        (
            "secp256k1_instruction_signature_count",
            stats.secp256k1_instruction_signature_count,
            i64
        ),
        (
            "ed25519_instruction_signature_count",
            stats.ed25519_instruction_signature_count,
            i64
        ),
        (
            "inflight_transaction_count",
            stats.in_flight_transaction_count,
            i64
        ),
        (
            "secp256r1_instruction_signature_count",
            stats.secp256r1_instruction_signature_count,
            i64
        ),
        ("total_transaction_fee", total_transaction_fee, i64),
        ("total_priority_fee", total_priority_fee, i64),
        (
            "number_of_contended_accounts",
            stats.number_of_contended_accounts,
            i64
        ),
    );
}

impl CostUpdateService {
    pub fn new(cost_update_receiver: CostUpdateReceiver) -> Self {
        let thread_hdl = Builder::new()
            .name("solCostUpdtSvc".to_string())
            .spawn(move || {
                Self::service_loop(cost_update_receiver);
            })
            .unwrap();

        Self { thread_hdl }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }

    fn service_loop(cost_update_receiver: CostUpdateReceiver) {
        for cost_update in cost_update_receiver.iter() {
            match cost_update {
                CostUpdate::FrozenBank {
                    bank,
                    is_leader_block,
                } => {
                    let (total_transaction_fee, total_priority_fee) = {
                        let collector_fee_details = bank.get_collector_fee_details();
                        (
                            collector_fee_details.total_transaction_fee(),
                            collector_fee_details.total_priority_fee(),
                        )
                    };
                    for loop_count in 1..=MAX_LOOP_COUNT {
                        {
                            // Release the lock so that the thread that will
                            // update the count is able to obtain a write lock
                            //
                            // Use inner scope to avoid sleeping with the lock
                            let cost_tracker = bank.read_cost_tracker().unwrap();
                            let in_flight_transaction_count =
                                cost_tracker.in_flight_transaction_count();

                            if in_flight_transaction_count == 0 || loop_count == MAX_LOOP_COUNT {
                                let slot = bank.slot();
                                trace!(
                                    "inflight transaction count is {in_flight_transaction_count} \
                                     for slot {slot} after {loop_count} iteration(s)"
                                );
                                report_cost_tracker_stats(
                                    &cost_tracker.stats(),
                                    slot,
                                    is_leader_block,
                                    total_transaction_fee,
                                    total_priority_fee,
                                );
                                break;
                            }
                        }
                        std::thread::sleep(LOOP_LIMITER);
                    }
                }
            }
        }
    }
}
