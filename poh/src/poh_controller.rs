use {
    crate::bank_message::BankMessage,
    crossbeam_channel::{unbounded, Receiver, SendError, Sender},
    solana_clock::Slot,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    std::sync::Arc,
};

/// Handle to control the Bank/slot that the PoH service is operating on.
#[derive(Clone)]
pub struct PohController {
    sender: Sender<BankMessage>,
}

impl PohController {
    pub fn new() -> (Self, Receiver<BankMessage>) {
        let (sender, receiver) = unbounded();
        (Self { sender }, receiver)
    }

    /// Signal to PoH to use a new bank.
    pub fn set_bank(
        &self,
        bank: BankWithScheduler,
        track_transaction_indexes: bool,
    ) -> Result<(), SendError<BankMessage>> {
        self.sender.send(BankMessage::SetBank {
            bank,
            track_transaction_indexes,
        })
    }

    /// Signal to reset PoH to specified bank.
    pub fn reset(
        &self,
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    ) -> Result<(), SendError<BankMessage>> {
        self.sender.send(BankMessage::Reset {
            reset_bank,
            next_leader_slot,
        })
    }
}
