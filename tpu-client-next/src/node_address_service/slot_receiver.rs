//! This module provides [`EstimatedSlot`] along with [`SlotReceiver`].
use {solana_clock::Slot, thiserror::Error, tokio::sync::watch};

/// Receiver for slot updates from slot update services.
#[derive(Clone)]
pub struct SlotReceiver {
    receiver: watch::Receiver<Slot>,
}

impl SlotReceiver {
    pub fn new(receiver: watch::Receiver<Slot>) -> Self {
        Self { receiver }
    }

    pub fn slot(&self) -> Slot {
        *self.receiver.borrow()
    }

    pub async fn changed(&mut self) -> Result<(), SlotReceiverError> {
        self.receiver
            .changed()
            .await
            .map_err(|_| SlotReceiverError::ChannelClosed)
    }
}

#[derive(Debug, Error)]
pub enum SlotReceiverError {
    #[error("Unexpectly dropped a channel.")]
    ChannelClosed,
}
