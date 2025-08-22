use {
    crossbeam_channel::{Receiver, SendError, Sender},
    solana_clock::Slot,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    std::sync::Arc,
};

pub enum PohServiceMessage {
    Reset {
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    },
    SetBank {
        bank: BankWithScheduler,
    },
}

/// Handle to control the Bank/slot that the PoH service is operating on.
#[derive(Clone)]
pub struct PohController {
    sender: Sender<PohServiceMessage>,
}

impl PohController {
    pub fn new() -> (Self, Receiver<PohServiceMessage>) {
        const CHANNEL_SIZE: usize = 16; // small size, we should never hit this.
        let (sender, receiver) = crossbeam_channel::bounded(CHANNEL_SIZE);
        (Self { sender }, receiver)
    }

    pub fn has_pending_message(&self) -> bool {
        !self.sender.is_empty()
    }

    /// Signal to PoH to use a new bank.
    pub fn set_bank_sync(
        &self,
        bank: BankWithScheduler,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_and_wait_on_pending_message(PohServiceMessage::SetBank { bank })
    }

    pub fn set_bank(&self, bank: BankWithScheduler) -> Result<(), SendError<PohServiceMessage>> {
        self.send_message(PohServiceMessage::SetBank { bank })
    }

    /// Signal to reset PoH to specified bank.
    pub fn reset_sync(
        &self,
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_and_wait_on_pending_message(PohServiceMessage::Reset {
            reset_bank,
            next_leader_slot,
        })
    }

    pub fn reset(
        &self,
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_message(PohServiceMessage::Reset {
            reset_bank,
            next_leader_slot,
        })
    }

    fn send_and_wait_on_pending_message(
        &self,
        message: PohServiceMessage,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_message(message)?;
        while self.has_pending_message() {
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn send_message(&self, message: PohServiceMessage) -> Result<(), SendError<PohServiceMessage>> {
        self.sender.send(message)?;
        Ok(())
    }
}
