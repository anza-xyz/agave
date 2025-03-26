use {
    super::immutable_deserialized_packet::{DeserializedPacketError, ImmutableDeserializedPacket},
    solana_perf::packet::Packet,
    std::{cmp::Ordering, sync::Arc},
};

/// Holds deserialized messages, as well as computed message_hash and other things needed to create
/// SanitizedTransaction
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeserializedPacket {
    immutable_section: Arc<ImmutableDeserializedPacket>,
}

impl DeserializedPacket {
    pub fn from_immutable_section(immutable_section: ImmutableDeserializedPacket) -> Self {
        Self {
            immutable_section: Arc::new(immutable_section),
        }
    }

    // add a ref
    pub fn new(packet: Packet) -> Result<Self, DeserializedPacketError> {
        let immutable_section = ImmutableDeserializedPacket::new(&packet)?;

        Ok(Self {
            immutable_section: Arc::new(immutable_section),
        })
    }

    pub fn immutable_section(&self) -> &Arc<ImmutableDeserializedPacket> {
        &self.immutable_section
    }
}

impl PartialOrd for DeserializedPacket {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DeserializedPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        self.immutable_section()
            .compute_unit_price()
            .cmp(&other.immutable_section().compute_unit_price())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_reserved_account_keys::ReservedAccountKeys,
        solana_perf::packet::PacketFlags,
        solana_runtime::bank::Bank,
        solana_sdk::{
            hash::Hash,
            signature::{Keypair, Signer},
            system_transaction,
            transaction::Transaction,
        },
        solana_vote::vote_transaction,
        solana_vote_program::vote_state::TowerSync,
    };

    #[cfg(test)]
    fn make_test_packets(
        transactions: Vec<Transaction>,
        vote_indexes: Vec<usize>,
    ) -> Vec<DeserializedPacket> {
        let capacity = transactions.len();
        let mut packet_vector = Vec::with_capacity(capacity);
        for tx in transactions.iter() {
            packet_vector.push(Packet::from_data(None, tx).unwrap());
        }
        for index in vote_indexes.iter() {
            packet_vector[*index].meta_mut().flags |= PacketFlags::SIMPLE_VOTE_TX;
        }

        packet_vector
            .into_iter()
            .map(|p| DeserializedPacket::new(p).unwrap())
            .collect()
    }

    #[test]
    fn test_transaction_from_deserialized_packet() {
        let keypair = Keypair::new();
        let transfer_tx =
            system_transaction::transfer(&keypair, &keypair.pubkey(), 1, Hash::default());
        let vote_tx = vote_transaction::new_tower_sync_transaction(
            TowerSync::from(vec![(42, 1)]),
            Hash::default(),
            &keypair,
            &keypair,
            &keypair,
            None,
        );
        let bank = Bank::default_for_tests();

        // packets with no votes
        {
            let vote_indexes = vec![];
            let packet_vector =
                make_test_packets(vec![transfer_tx.clone(), transfer_tx.clone()], vote_indexes);

            let mut votes_only = false;
            let txs = packet_vector.iter().filter_map(|tx| {
                tx.immutable_section().build_sanitized_transaction(
                    votes_only,
                    &bank,
                    &ReservedAccountKeys::empty_key_set(),
                )
            });
            assert_eq!(2, txs.count());

            votes_only = true;
            let txs = packet_vector.iter().filter_map(|tx| {
                tx.immutable_section().build_sanitized_transaction(
                    votes_only,
                    &bank,
                    &ReservedAccountKeys::empty_key_set(),
                )
            });
            assert_eq!(0, txs.count());
        }

        // packets with some votes
        {
            let vote_indexes = vec![0, 2];
            let packet_vector = make_test_packets(
                vec![vote_tx.clone(), transfer_tx, vote_tx.clone()],
                vote_indexes,
            );

            let mut votes_only = false;
            let txs = packet_vector.iter().filter_map(|tx| {
                tx.immutable_section().build_sanitized_transaction(
                    votes_only,
                    &bank,
                    &ReservedAccountKeys::empty_key_set(),
                )
            });
            assert_eq!(3, txs.count());

            votes_only = true;
            let txs = packet_vector.iter().filter_map(|tx| {
                tx.immutable_section().build_sanitized_transaction(
                    votes_only,
                    &bank,
                    &ReservedAccountKeys::empty_key_set(),
                )
            });
            assert_eq!(2, txs.count());
        }

        // packets with all votes
        {
            let vote_indexes = vec![0, 1, 2];
            let packet_vector = make_test_packets(
                vec![vote_tx.clone(), vote_tx.clone(), vote_tx],
                vote_indexes,
            );

            let mut votes_only = false;
            let txs = packet_vector.iter().filter_map(|tx| {
                tx.immutable_section().build_sanitized_transaction(
                    votes_only,
                    &bank,
                    &ReservedAccountKeys::empty_key_set(),
                )
            });
            assert_eq!(3, txs.count());

            votes_only = true;
            let txs = packet_vector.iter().filter_map(|tx| {
                tx.immutable_section().build_sanitized_transaction(
                    votes_only,
                    &bank,
                    &ReservedAccountKeys::empty_key_set(),
                )
            });
            assert_eq!(3, txs.count());
        }
    }
}
