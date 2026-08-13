use {
    crate::common::nonblocking_send, crossbeam_channel::Sender, solana_clock::Slot,
    solana_pubkey::Pubkey,
};

#[derive(Debug, PartialEq, Eq)]
pub enum CommitmentType {
    /// Our node has voted notarize for the slot
    Notarize,
    /// Votor has selected the slot as root
    Rooted,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommitmentAggregationData {
    pub commitment_type: CommitmentType,
    pub slot: Slot,
    pub dependency_work: Option<u64>,
}

pub fn update_commitment_cache(
    my_pubkey: &Pubkey,
    commitment_type: CommitmentType,
    slot: Slot,
    dependency_work: Option<u64>,
    sender: &Sender<CommitmentAggregationData>,
) {
    let msg = CommitmentAggregationData {
        commitment_type,
        slot,
        dependency_work,
    };
    if let Err(channel_name) = nonblocking_send(my_pubkey, sender, msg, "commitment_sender") {
        warn!("{my_pubkey}: channel \"{channel_name}\" disconnected");
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crossbeam_channel::bounded};

    #[test]
    fn test_update_commitment_cache() {
        let (commitment_sender, commitment_receiver) = bounded(1024);
        let slot = 3;
        let commitment_type = CommitmentType::Notarize;
        let my_pubkey = Pubkey::new_unique();
        update_commitment_cache(&my_pubkey, commitment_type, slot, None, &commitment_sender);
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            CommitmentAggregationData {
                commitment_type: CommitmentType::Notarize,
                slot,
                dependency_work: None,
            }
        );
        let slot = 5;
        let commitment_type = CommitmentType::Rooted;
        let dependency_work = Some(42);
        update_commitment_cache(
            &my_pubkey,
            commitment_type,
            slot,
            dependency_work,
            &commitment_sender,
        );
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            CommitmentAggregationData {
                commitment_type: CommitmentType::Rooted,
                slot,
                dependency_work,
            }
        );
    }
}
