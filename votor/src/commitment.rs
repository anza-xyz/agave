use {
    crossbeam_channel::{Sender, TrySendError},
    solana_clock::Slot,
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum AlpenglowCommitmentError {
    #[error("Failed to send commitment data, channel disconnected")]
    ChannelDisconnected,
}

pub enum AlpenglowCommitmentType {
    /// Our node has voted notarize for the slot
    Notarize,
    /// We have observed a finalization certificate for the slot
    Finalized,
}

pub struct AlpenglowCommitmentAggregationData {
    pub commitment_type: AlpenglowCommitmentType,
    pub slot: Slot,
}

pub fn alpenglow_update_commitment_cache(
    commitment_type: AlpenglowCommitmentType,
    slot: Slot,
    commitment_sender: &Sender<AlpenglowCommitmentAggregationData>,
) -> Result<(), AlpenglowCommitmentError> {
    match commitment_sender.try_send(AlpenglowCommitmentAggregationData {
        commitment_type,
        slot,
    }) {
        Err(TrySendError::Disconnected(_)) => {
            info!("commitment_sender has disconnected");
            return Err(AlpenglowCommitmentError::ChannelDisconnected);
        }
        Err(TrySendError::Full(_)) => error!("commitment_sender is backed up, something is wrong"),
        Ok(_) => (),
    }
    Ok(())
}
