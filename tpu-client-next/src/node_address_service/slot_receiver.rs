use {
    crate::node_address_service::NodeAddressServiceError, solana_clock::Slot, tokio::sync::watch,
};

pub type Timestamp = u64;

#[derive(Clone)]
pub struct SlotReceiver {
    receiver: watch::Receiver<(Slot, Timestamp)>,
}

impl SlotReceiver {
    pub fn new(receiver: watch::Receiver<(Slot, Timestamp)>) -> Self {
        Self { receiver }
    }

    pub fn slot_with_timestamp(&self) -> (Slot, Timestamp) {
        *self.receiver.borrow()
    }

    pub async fn changed(&mut self) -> Result<(), NodeAddressServiceError> {
        self.receiver
            .changed()
            .await
            .map_err(|_| NodeAddressServiceError::ChannelClosed)
    }
}
