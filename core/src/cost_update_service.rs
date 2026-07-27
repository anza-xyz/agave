//! this service asynchronously reports CostTracker stats

use {
    crossbeam_channel::Receiver,
    solana_runtime::bank::Bank,
    std::{
        sync::Arc,
        thread::{self, Builder, JoinHandle},
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
                    let cost_tracker = bank.read_cost_tracker().unwrap();
                    cost_tracker.report_stats(
                        bank.slot(),
                        is_leader_block,
                        total_transaction_fee,
                        total_priority_fee,
                    );
                }
            }
        }
    }
}
