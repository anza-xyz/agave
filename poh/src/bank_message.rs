use {
    solana_clock::Slot,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    std::sync::Arc,
};

/// Communication from controller to PoH on how to set/reset the Bank.
pub enum BankMessage {
    Reset {
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    },
    SetBank {
        bank: BankWithScheduler,
        track_transaction_indexes: bool,
    },
}
