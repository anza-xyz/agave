#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crossbeam_channel::{Receiver, SendError, Sender, TryRecvError},
    solana_clock::Slot,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
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
pub struct PohController {
    sender: Sender<PohServiceMessage>,
    /// Used to indicate if there are any pending messages in the channel
    /// OR that the receiver is currently processing.
    /// This is necessary because crossbeam does not support peeking the
    /// channel.
    pending_message: Arc<AtomicUsize>,
    track_last_bank: bool,
    last_bank: Option<BankWithScheduler>,
}

impl PohController {
    pub fn new(track_last_bank: bool) -> (Self, PohServiceMessageReceiver) {
        const CHANNEL_SIZE: usize = 16; // small size, we should never hit this.
        let (sender, receiver) = crossbeam_channel::bounded(CHANNEL_SIZE);
        let pending_message = Arc::new(AtomicUsize::new(0));
        let receiver = PohServiceMessageReceiver {
            receiver,
            pending_message: pending_message.clone(),
        };
        (
            Self {
                sender,
                pending_message,
                track_last_bank,
                last_bank: None,
            },
            receiver,
        )
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new_for_test() -> (Self, PohServiceMessageReceiver) {
        Self::new(false)
    }

    pub fn has_pending_message(&self) -> bool {
        self.pending_message.load(Ordering::Acquire) > 0
    }

    /// Signal to PoH to use a new bank.
    pub fn set_bank_sync(
        &mut self,
        bank: BankWithScheduler,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_and_wait_on_pending_message(PohServiceMessage::SetBank { bank })
    }

    pub fn set_bank(
        &mut self,
        bank: BankWithScheduler,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_message(PohServiceMessage::SetBank { bank })
    }

    /// Signal to reset PoH to specified bank.
    pub fn reset_sync(
        &mut self,
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_and_wait_on_pending_message(PohServiceMessage::Reset {
            reset_bank,
            next_leader_slot,
        })
    }

    pub fn reset(
        &mut self,
        reset_bank: Arc<Bank>,
        next_leader_slot: Option<(Slot, Slot)>,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_message(PohServiceMessage::Reset {
            reset_bank,
            next_leader_slot,
        })
    }

    fn send_and_wait_on_pending_message(
        &mut self,
        message: PohServiceMessage,
    ) -> Result<(), SendError<PohServiceMessage>> {
        self.send_message(message)?;
        while self.has_pending_message() {
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn send_message(
        &mut self,
        message: PohServiceMessage,
    ) -> Result<(), SendError<PohServiceMessage>> {
        let cleared_bank = if self.track_last_bank {
            match &message {
                PohServiceMessage::SetBank { bank } => {
                    self.last_bank = Some(bank.clone_with_scheduler());
                    None
                }
                PohServiceMessage::Reset { .. } => self.last_bank.take(),
            }
        } else {
            None
        };

        self.pending_message.fetch_add(1, Ordering::AcqRel);
        self.sender.send(message)?;

        if let Some(cleared_bank) = cleared_bank {
            // This must be done without poh_recorder lock being held; otherwise deadlock would
            // occur. So, we can't nicely hide this impl detail inside PohRecorder::reset()..
            cleared_bank.ensure_return_abandoned_bp_scheduler_to_scheduler_pool();
        }

        Ok(())
    }
}

pub struct PohServiceMessageReceiver {
    receiver: Receiver<PohServiceMessage>,
    /// Used to indicate if there are any pending messages in the channel
    /// OR that the receiver is currently processing.
    /// This is necessary because crossbeam does not support peeking the
    /// channel.
    pending_message: Arc<AtomicUsize>,
}

impl PohServiceMessageReceiver {
    pub(crate) fn try_recv(&self) -> Result<PohServiceMessageGuard, TryRecvError> {
        self.receiver
            .try_recv()
            .map(|message| PohServiceMessageGuard {
                message_receiver: self,
                message: Some(message),
            })
    }
}

pub(crate) struct PohServiceMessageGuard<'a> {
    message_receiver: &'a PohServiceMessageReceiver,
    message: Option<PohServiceMessage>,
}

impl PohServiceMessageGuard<'_> {
    pub(crate) fn take(&mut self) -> PohServiceMessage {
        self.message.take().unwrap()
    }
}

impl Drop for PohServiceMessageGuard<'_> {
    fn drop(&mut self) {
        // If the message was taken (processed), decrement the pending count.
        if self.message.is_none() {
            self.message_receiver
                .pending_message
                .fetch_sub(1, Ordering::AcqRel);
        } else {
            panic!("PohServiceMessageGuard dropped without processing the message");
        }
    }
}
