//! This module provides [`SlotReceiver`] structure.
use {solana_clock::Slot, thiserror::Error, tokio::sync::watch};

/// Receiver for slot updates from slot update services.
#[derive(Clone)]
pub struct SlotReceiver(watch::Receiver<Slot>);

impl SlotReceiver {
    pub fn new(receiver: watch::Receiver<Slot>) -> Self {
        Self(receiver)
    }

    pub fn slot(&self) -> Slot {
        *self.0.borrow()
    }

    pub async fn changed(&mut self) -> Result<(), SlotReceiverError> {
        self.0
            .changed()
            .await
            .map_err(|_| SlotReceiverError::ChannelClosed)
    }
}

#[derive(Debug, Error)]
pub enum SlotReceiverError {
    #[error("Unexpectedly dropped a channel.")]
    ChannelClosed,
}
