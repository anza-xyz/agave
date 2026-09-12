//! This module provides [`SlotReceiver`] structure.
use {crate::leader_updater::SlotEstimate, thiserror::Error, tokio::sync::watch};

/// Receiver for slot updates from slot update services.
#[derive(Clone)]
pub struct SlotReceiver(watch::Receiver<SlotEstimate>);

impl SlotReceiver {
    pub fn new(receiver: watch::Receiver<SlotEstimate>) -> Self {
        Self(receiver)
    }

    pub fn slot(&self) -> SlotEstimate {
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
