use {
    crossbeam_channel::{Sender, TrySendError},
    solana_clock::Slot,
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum CommitmentError {
    #[error("Failed to send commitment data, channel disconnected")]
    ChannelDisconnected,
}

#[derive(Debug, PartialEq)]
pub enum CommitmentType {
    /// Our node has voted notarize for the slot
    Notarize,
    /// We have observed a finalization certificate for the slot
    Finalized,
}

#[derive(Debug, PartialEq)]
pub struct CommitmentAggregationData {
    pub commitment_type: CommitmentType,
    pub slot: Slot,
}

pub fn update_commitment_cache(
    commitment_type: CommitmentType,
    slot: Slot,
    commitment_sender: &Sender<CommitmentAggregationData>,
) -> Result<(), CommitmentError> {
    match commitment_sender.try_send(CommitmentAggregationData {
        commitment_type,
        slot,
    }) {
        Err(TrySendError::Disconnected(_)) => {
            info!("commitment_sender has disconnected");
            return Err(CommitmentError::ChannelDisconnected);
        }
        Err(TrySendError::Full(_)) => error!("commitment_sender is backed up, something is wrong"),
        Ok(_) => (),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, crossbeam_channel::unbounded};

    #[test]
    fn test_update_commitment_cache() {
        let (commitment_sender, commitment_receiver) = unbounded();
        let slot = 3;
        let commitment_type = CommitmentType::Notarize;
        update_commitment_cache(commitment_type, slot, &commitment_sender)
            .expect("Failed to send commitment data");
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            CommitmentAggregationData {
                commitment_type: CommitmentType::Notarize,
                slot,
            }
        );
        let slot = 5;
        let commitment_type = CommitmentType::Finalized;
        update_commitment_cache(commitment_type, slot, &commitment_sender)
            .expect("Failed to send commitment data");
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            CommitmentAggregationData {
                commitment_type: CommitmentType::Finalized,
                slot,
            }
        );

        // Close the channel and ensure the error is returned
        drop(commitment_receiver);
        let result = update_commitment_cache(CommitmentType::Notarize, 7, &commitment_sender);
        assert!(matches!(result, Err(CommitmentError::ChannelDisconnected)));
    }
}
