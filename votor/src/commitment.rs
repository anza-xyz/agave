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

#[derive(Debug, PartialEq)]
pub enum AlpenglowCommitmentType {
    /// Our node has voted notarize for the slot
    Notarize,
    /// We have observed a finalization certificate for the slot
    Finalized,
}

#[derive(Debug, PartialEq)]
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

#[cfg(test)]
mod tests {
    use {super::*, crossbeam_channel::unbounded};

    #[test]
    fn test_alpenglow_update_commitment_cache() {
        let (commitment_sender, commitment_receiver) = unbounded();
        let slot = 3;
        let commitment_type = AlpenglowCommitmentType::Notarize;
        alpenglow_update_commitment_cache(commitment_type, slot, &commitment_sender)
            .expect("Failed to send commitment data");
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            AlpenglowCommitmentAggregationData {
                commitment_type: AlpenglowCommitmentType::Notarize,
                slot,
            }
        );
        let slot = 5;
        let commitment_type = AlpenglowCommitmentType::Finalized;
        alpenglow_update_commitment_cache(commitment_type, slot, &commitment_sender)
            .expect("Failed to send commitment data");
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            AlpenglowCommitmentAggregationData {
                commitment_type: AlpenglowCommitmentType::Finalized,
                slot,
            }
        );

        // Close the channel and ensure the error is returned
        drop(commitment_receiver);
        let result = alpenglow_update_commitment_cache(
            AlpenglowCommitmentType::Notarize,
            7,
            &commitment_sender,
        );
        assert!(matches!(
            result,
            Err(AlpenglowCommitmentError::ChannelDisconnected)
        ));
    }
}
