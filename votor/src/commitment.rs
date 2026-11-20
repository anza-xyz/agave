use {
    crate::common::nonblocking_send, crossbeam_channel::Sender, solana_clock::Slot,
    solana_pubkey::Pubkey, solana_runtime::bank::BankId,
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
    pub bank_id: BankId,
}

pub fn update_commitment_cache(
    my_pubkey: &Pubkey,
    commitment_type: CommitmentType,
    slot: Slot,
    bank_id: BankId,
    sender: &Sender<CommitmentAggregationData>,
) {
    let msg = CommitmentAggregationData {
        commitment_type,
        slot,
        bank_id,
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
        let bank_id = BankId::new(42);
        let commitment_type = CommitmentType::Notarize;
        let my_pubkey = Pubkey::new_unique();
        update_commitment_cache(
            &my_pubkey,
            commitment_type,
            slot,
            bank_id,
            &commitment_sender,
        );
        let received_data = commitment_receiver
            .try_recv()
            .expect("Failed to receive commitment data");
        assert_eq!(
            received_data,
            CommitmentAggregationData {
                commitment_type: CommitmentType::Notarize,
                slot,
                bank_id,
            }
        );
        let slot = 5;
        let bank_id = BankId::new(43);
        let commitment_type = CommitmentType::Rooted;
        update_commitment_cache(
            &my_pubkey,
            commitment_type,
            slot,
            bank_id,
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
                bank_id,
            }
        );
    }
}
