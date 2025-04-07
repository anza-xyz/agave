use {
    crate::bank_message::BankMessage,
    crossbeam_channel::{unbounded, Receiver, SendError, Sender},
    solana_clock::Slot,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// Handle to control the Bank/slot that the PoH service is operating on.
#[derive(Clone)]
pub struct PohController {
    sender: Sender<BankMessage>,
    pending_message: Arc<AtomicBool>,
}

impl PohController {
    pub fn new() -> (Self, Receiver<BankMessage>) {
        let (sender, receiver) = unbounded();
        (
            Self {
                sender,
                pending_message: Arc::default(),
            },
            receiver,
        )
    }

    pub fn pending_message(&self) -> Arc<AtomicBool> {
        self.pending_message.clone()
    }

    /// Signal to PoH to use a new bank.
    pub fn set_bank(
        &self,
        bank: BankWithScheduler,
        track_transaction_indexes: bool,
    ) -> Result<(), SendError<BankMessage>> {
        self.send_and_wait_on_pending_message(BankMessage::SetBank {
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
        self.send_and_wait_on_pending_message(BankMessage::Reset {
            reset_bank,
            next_leader_slot,
        })
    }

    fn send_and_wait_on_pending_message(
        &self,
        message: BankMessage,
    ) -> Result<(), SendError<BankMessage>> {
        self.pending_message.store(true, Ordering::Release);
        self.sender.send(message)?;
        while self.pending_message.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Ok(())
    }
}
