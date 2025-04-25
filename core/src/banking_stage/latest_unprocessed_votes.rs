use {
    super::immutable_deserialized_packet::{DeserializedPacketError, ImmutableDeserializedPacket},
    agave_feature_set as feature_set,
    itertools::Itertools,
    rand::{thread_rng, Rng},
    solana_perf::packet::Packet,
    solana_runtime::{bank::Bank, epoch_stakes::EpochStakes},
    solana_sdk::{
        account::from_account,
        clock::{Epoch, Slot, UnixTimestamp},
        hash::Hash,
        program_utils::limited_deserialize,
        pubkey::Pubkey,
        slot_hashes::SlotHashes,
        sysvar,
    },
    solana_vote_program::vote_instruction::VoteInstruction,
    std::{cmp, collections::HashMap, sync::Arc},
};

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum VoteSource {
    Gossip,
    Tpu,
}

/// Holds deserialized vote messages as well as their source, and slot
#[derive(Debug, Clone)]
pub struct LatestValidatorVotePacket {
    vote_source: VoteSource,
    vote_pubkey: Pubkey,
    vote: Option<Arc<ImmutableDeserializedPacket>>,
    slot: Slot,
    hash: Hash,
    timestamp: Option<UnixTimestamp>,
}

impl LatestValidatorVotePacket {
    pub fn new(
        packet: &Packet,
        vote_source: VoteSource,
        deprecate_legacy_vote_ixs: bool,
    ) -> Result<Self, DeserializedPacketError> {
        if !packet.meta().is_simple_vote_tx() {
            return Err(DeserializedPacketError::VoteTransactionError);
        }

        let vote = Arc::new(ImmutableDeserializedPacket::new(packet)?);
        Self::new_from_immutable(vote, vote_source, deprecate_legacy_vote_ixs)
    }

    pub fn new_from_immutable(
        vote: Arc<ImmutableDeserializedPacket>,
        vote_source: VoteSource,
        deprecate_legacy_vote_ixs: bool,
    ) -> Result<Self, DeserializedPacketError> {
        let message = vote.transaction().get_message();
        let (_, instruction) = message
            .program_instructions_iter()
            .next()
            .ok_or(DeserializedPacketError::VoteTransactionError)?;

        let instruction_filter = |ix: &VoteInstruction| {
            if deprecate_legacy_vote_ixs {
                matches!(
                    ix,
                    VoteInstruction::TowerSync(_) | VoteInstruction::TowerSyncSwitch(_, _),
                )
            } else {
                ix.is_single_vote_state_update()
            }
        };

        match limited_deserialize::<VoteInstruction>(&instruction.data) {
            Ok(vote_state_update_instruction)
                if instruction_filter(&vote_state_update_instruction) =>
            {
                let vote_account_index = instruction
                    .accounts
                    .first()
                    .copied()
                    .ok_or(DeserializedPacketError::VoteTransactionError)?;
                let vote_pubkey = message
                    .message
                    .static_account_keys()
                    .get(vote_account_index as usize)
                    .copied()
                    .ok_or(DeserializedPacketError::VoteTransactionError)?;
                let slot = vote_state_update_instruction.last_voted_slot().unwrap_or(0);
                let hash = vote_state_update_instruction.hash();
                let timestamp = vote_state_update_instruction.timestamp();

                Ok(Self {
                    vote: Some(vote),
                    slot,
                    hash,
                    vote_pubkey,
                    vote_source,
                    timestamp,
                })
            }
            _ => Err(DeserializedPacketError::VoteTransactionError),
        }
    }

    pub fn get_vote_packet(&self) -> Arc<ImmutableDeserializedPacket> {
        self.vote.as_ref().unwrap().clone()
    }

    pub fn vote_pubkey(&self) -> Pubkey {
        self.vote_pubkey
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub(crate) fn hash(&self) -> Hash {
        self.hash
    }

    pub fn timestamp(&self) -> Option<UnixTimestamp> {
        self.timestamp
    }

    pub fn is_vote_taken(&self) -> bool {
        self.vote.is_none()
    }

    pub fn take_vote(&mut self) -> Option<Arc<ImmutableDeserializedPacket>> {
        self.vote.take()
    }
}

#[derive(Default, Debug)]
pub(crate) struct VoteBatchInsertionMetrics {
    pub(crate) num_dropped_gossip: usize,
    pub(crate) num_dropped_tpu: usize,
}

impl VoteBatchInsertionMetrics {
    pub fn total_dropped_packets(&self) -> usize {
        self.num_dropped_gossip + self.num_dropped_tpu
    }

    pub fn dropped_gossip_packets(&self) -> usize {
        self.num_dropped_gossip
    }

    pub fn dropped_tpu_packets(&self) -> usize {
        self.num_dropped_tpu
    }
}

#[derive(Debug)]
pub struct LatestUnprocessedVotes {
    latest_vote_per_vote_pubkey: HashMap<Pubkey, LatestValidatorVotePacket>,
    num_unprocessed_votes: usize,
    cached_epoch_stakes: EpochStakes,
    deprecate_legacy_vote_ixs: bool,
    current_epoch: Epoch,
}

impl LatestUnprocessedVotes {
    pub fn new(bank: &Bank) -> Self {
        let deprecate_legacy_vote_ixs = bank
            .feature_set
            .is_active(&feature_set::deprecate_legacy_vote_ixs::id());
        Self {
            latest_vote_per_vote_pubkey: HashMap::default(),
            num_unprocessed_votes: 0,
            cached_epoch_stakes: bank.current_epoch_stakes().clone(),
            current_epoch: bank.epoch(),
            deprecate_legacy_vote_ixs,
        }
    }

    #[cfg(test)]
    pub fn new_for_tests(vote_pubkeys_to_stake: &[Pubkey]) -> Self {
        use solana_vote::vote_account::VoteAccount;

        let vote_accounts = vote_pubkeys_to_stake
            .iter()
            .map(|pubkey| (*pubkey, (1u64, VoteAccount::new_random())))
            .collect();
        let epoch_stakes = EpochStakes::new_for_tests(vote_accounts, 0);

        Self {
            latest_vote_per_vote_pubkey: HashMap::default(),
            num_unprocessed_votes: 0,
            cached_epoch_stakes: epoch_stakes,
            current_epoch: 0,
            deprecate_legacy_vote_ixs: true,
        }
    }

    pub fn len(&self) -> usize {
        self.num_unprocessed_votes
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn insert_batch(
        &mut self,
        votes: impl Iterator<Item = LatestValidatorVotePacket>,
        should_replenish_taken_votes: bool,
    ) -> VoteBatchInsertionMetrics {
        let mut num_dropped_gossip = 0;
        let mut num_dropped_tpu = 0;

        for vote in votes {
            if self
                .cached_epoch_stakes
                .vote_account_stake(&vote.vote_pubkey())
                == 0
            {
                continue;
            }

            if let Some(vote) = self.update_latest_vote(vote, should_replenish_taken_votes) {
                match vote.vote_source {
                    VoteSource::Gossip => num_dropped_gossip += 1,
                    VoteSource::Tpu => num_dropped_tpu += 1,
                }
            }
        }

        VoteBatchInsertionMetrics {
            num_dropped_gossip,
            num_dropped_tpu,
        }
    }

    /// If this vote causes an unprocessed vote to be removed, returns Some(old_vote)
    /// If there is a newer vote processed / waiting to be processed returns Some(vote)
    /// Otherwise returns None
    pub fn update_latest_vote(
        &mut self,
        vote: LatestValidatorVotePacket,
        should_replenish_taken_votes: bool,
    ) -> Option<LatestValidatorVotePacket> {
        let vote_pubkey = vote.vote_pubkey();
        // Grab write-lock to insert new vote.
        match self.latest_vote_per_vote_pubkey.entry(vote_pubkey) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let latest_vote = entry.get_mut();
                if Self::allow_update(&vote, latest_vote, should_replenish_taken_votes) {
                    let old_vote = std::mem::replace(latest_vote, vote);
                    if old_vote.is_vote_taken() {
                        self.num_unprocessed_votes += 1;
                        return None;
                    } else {
                        return Some(old_vote);
                    }
                }
                Some(vote)
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(vote);
                self.num_unprocessed_votes += 1;
                None
            }
        }
    }

    #[cfg(test)]
    pub fn get_latest_vote_slot(&self, pubkey: Pubkey) -> Option<Slot> {
        self.latest_vote_per_vote_pubkey
            .get(&pubkey)
            .map(|l| l.slot())
    }

    #[cfg(test)]
    fn get_latest_timestamp(&self, pubkey: Pubkey) -> Option<UnixTimestamp> {
        self.latest_vote_per_vote_pubkey
            .get(&pubkey)
            .and_then(|l| l.timestamp())
    }

    fn weighted_random_order_by_stake(&self) -> impl Iterator<Item = Pubkey> {
        // Efraimidis and Spirakis algo for weighted random sample without replacement
        let mut pubkey_with_weight: Vec<(f64, Pubkey)> = self
            .latest_vote_per_vote_pubkey
            .keys()
            .filter_map(|&pubkey| {
                let stake = self.cached_epoch_stakes.vote_account_stake(&pubkey);
                if stake == 0 {
                    None // Ignore votes from unstaked validators
                } else {
                    Some((thread_rng().gen::<f64>().powf(1.0 / (stake as f64)), pubkey))
                }
            })
            .collect::<Vec<_>>();
        pubkey_with_weight.sort_by(|(w1, _), (w2, _)| w1.partial_cmp(w2).unwrap());
        pubkey_with_weight.into_iter().map(|(_, pubkey)| pubkey)
    }

    /// Recache the staked nodes based on a bank from the new epoch.
    /// This should only be run by the TPU vote thread
    pub(super) fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        if bank.epoch() <= self.current_epoch {
            return;
        }
        {
            self.cached_epoch_stakes = bank.current_epoch_stakes().clone();
            self.current_epoch = bank.epoch();
            self.deprecate_legacy_vote_ixs = bank
                .feature_set
                .is_active(&feature_set::deprecate_legacy_vote_ixs::id());
        }

        // Evict any now unstaked pubkeys
        let mut unstaked_votes = 0;
        self.latest_vote_per_vote_pubkey
            .retain(|vote_pubkey, vote| {
                let is_present = !vote.is_vote_taken();
                let should_evict = self.cached_epoch_stakes.vote_account_stake(vote_pubkey) == 0;
                if is_present && should_evict {
                    unstaked_votes += 1;
                }
                !should_evict
            });
        self.num_unprocessed_votes -= unstaked_votes;
        datapoint_info!(
            "latest_unprocessed_votes-epoch-boundary",
            ("epoch", bank.epoch(), i64),
            ("evicted_unstaked_votes", unstaked_votes, i64)
        );
    }

    /// Drains all votes yet to be processed sorted by a weighted random ordering by stake
    /// Do not touch votes that are for a different fork from `bank` as we know they will fail,
    /// however the next bank could be built on a different fork and consume these votes.
    pub fn drain_unprocessed(&mut self, bank: &Bank) -> Vec<Arc<ImmutableDeserializedPacket>> {
        let slot_hashes = bank
            .get_account(&sysvar::slot_hashes::id())
            .and_then(|account| from_account::<SlotHashes, _>(&account));
        if slot_hashes.is_none() {
            error!(
                "Slot hashes sysvar doesn't exist on bank {}. Including all votes without \
                 filtering",
                bank.slot()
            );
        }

        self.weighted_random_order_by_stake()
            .filter_map(|pubkey| {
                self.latest_vote_per_vote_pubkey
                    .get_mut(&pubkey)
                    .and_then(|latest_vote| {
                        if !Self::is_valid_for_our_fork(latest_vote, &slot_hashes) {
                            return None;
                        }
                        latest_vote.take_vote().inspect(|_vote| {
                            self.num_unprocessed_votes -= 1;
                        })
                    })
            })
            .collect_vec()
    }

    /// Check if `vote` can land in our fork based on `slot_hashes`
    fn is_valid_for_our_fork(
        vote: &LatestValidatorVotePacket,
        slot_hashes: &Option<SlotHashes>,
    ) -> bool {
        let Some(slot_hashes) = slot_hashes else {
            // When slot hashes is not present we do not filter
            return true;
        };
        slot_hashes
            .get(&vote.slot())
            .map(|found_hash| *found_hash == vote.hash())
            .unwrap_or(false)
    }

    pub fn clear(&mut self) {
        self.latest_vote_per_vote_pubkey
            .values_mut()
            .for_each(|vote| {
                if vote.take_vote().is_some() {
                    self.num_unprocessed_votes -= 1;
                }
            });
    }

    pub(super) fn should_deprecate_legacy_vote_ixs(&self) -> bool {
        self.deprecate_legacy_vote_ixs
    }

    /// Allow votes for later slots or the same slot with later timestamp (refreshed votes)
    /// We directly compare as options to prioritize votes for same slot with timestamp as
    /// Some > None
    fn allow_update(
        vote: &LatestValidatorVotePacket,
        latest_vote: &LatestValidatorVotePacket,
        should_replenish_taken_votes: bool,
    ) -> bool {
        let slot = vote.slot();

        match slot.cmp(&latest_vote.slot()) {
            cmp::Ordering::Less => return false,
            cmp::Ordering::Greater => return true,
            cmp::Ordering::Equal => {}
        };

        // Slots are equal, now check timestamp
        match vote.timestamp().cmp(&latest_vote.timestamp()) {
            cmp::Ordering::Less => return false,
            cmp::Ordering::Greater => return true,
            cmp::Ordering::Equal => {}
        };

        // Timestamps are equal, lastly check if vote was taken previously
        // and should be replenished
        should_replenish_taken_votes && latest_vote.is_vote_taken()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        solana_perf::packet::{Packet, PacketBatch, PacketFlags},
        solana_runtime::{
            bank::Bank,
            genesis_utils::{self, ValidatorVoteKeypairs},
        },
        solana_sdk::{
            epoch_schedule::MINIMUM_SLOTS_PER_EPOCH, genesis_config::GenesisConfig, hash::Hash,
            signature::Signer, system_transaction::transfer,
        },
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::sync::Arc,
    };

    fn from_slots(
        slots: Vec<(u64, u32)>,
        vote_source: VoteSource,
        keypairs: &ValidatorVoteKeypairs,
        timestamp: Option<UnixTimestamp>,
    ) -> LatestValidatorVotePacket {
        let mut vote = TowerSync::from(slots);
        vote.timestamp = timestamp;
        let vote_tx = new_tower_sync_transaction(
            vote,
            Hash::new_unique(),
            &keypairs.node_keypair,
            &keypairs.vote_keypair,
            &keypairs.vote_keypair,
            None,
        );
        let mut packet = Packet::from_data(None, vote_tx).unwrap();
        packet
            .meta_mut()
            .flags
            .set(PacketFlags::SIMPLE_VOTE_TX, true);
        LatestValidatorVotePacket::new(&packet, vote_source, true).unwrap()
    }

    fn deserialize_packets<'a>(
        packet_batch: &'a PacketBatch,
        packet_indexes: &'a [usize],
        vote_source: VoteSource,
    ) -> impl Iterator<Item = LatestValidatorVotePacket> + 'a {
        packet_indexes.iter().filter_map(move |packet_index| {
            LatestValidatorVotePacket::new(&packet_batch[*packet_index], vote_source, true).ok()
        })
    }

    #[test]
    fn test_deserialize_vote_packets() {
        let keypairs = ValidatorVoteKeypairs::new_rand();
        let blockhash = Hash::new_unique();
        let switch_proof = Hash::new_unique();
        let mut tower_sync = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::from(vec![(0, 3), (1, 2), (2, 1)]),
                blockhash,
                &keypairs.node_keypair,
                &keypairs.vote_keypair,
                &keypairs.vote_keypair,
                None,
            ),
        )
        .unwrap();
        tower_sync
            .meta_mut()
            .flags
            .set(PacketFlags::SIMPLE_VOTE_TX, true);
        let mut tower_sync_switch = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::from(vec![(0, 3), (1, 2), (3, 1)]),
                blockhash,
                &keypairs.node_keypair,
                &keypairs.vote_keypair,
                &keypairs.vote_keypair,
                Some(switch_proof),
            ),
        )
        .unwrap();
        tower_sync_switch
            .meta_mut()
            .flags
            .set(PacketFlags::SIMPLE_VOTE_TX, true);
        let random_transaction = Packet::from_data(
            None,
            transfer(
                &keypairs.node_keypair,
                &Pubkey::new_unique(),
                1000,
                blockhash,
            ),
        )
        .unwrap();
        let packet_batch =
            PacketBatch::new(vec![tower_sync, tower_sync_switch, random_transaction]);

        let deserialized_packets = deserialize_packets(
            &packet_batch,
            &(0..packet_batch.len()).collect_vec(),
            VoteSource::Gossip,
        )
        .collect_vec();

        assert_eq!(2, deserialized_packets.len());
        assert_eq!(VoteSource::Gossip, deserialized_packets[0].vote_source);
        assert_eq!(VoteSource::Gossip, deserialized_packets[1].vote_source);

        assert_eq!(
            keypairs.vote_keypair.pubkey(),
            deserialized_packets[0].vote_pubkey
        );
        assert_eq!(
            keypairs.vote_keypair.pubkey(),
            deserialized_packets[1].vote_pubkey
        );

        assert!(deserialized_packets[0].vote.is_some());
        assert!(deserialized_packets[1].vote.is_some());
    }

    #[test]
    fn test_update_latest_vote() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let keypair_b = ValidatorVoteKeypairs::new_rand();
        let mut latest_unprocessed_votes = LatestUnprocessedVotes::new_for_tests(&[
            keypair_a.vote_keypair.pubkey(),
            keypair_b.vote_keypair.pubkey(),
        ]);

        let vote_a = from_slots(vec![(0, 2), (1, 1)], VoteSource::Gossip, &keypair_a, None);
        let vote_b = from_slots(
            vec![(0, 5), (4, 2), (9, 1)],
            VoteSource::Gossip,
            &keypair_b,
            None,
        );

        assert!(latest_unprocessed_votes
            .update_latest_vote(vote_a, false /* should replenish */)
            .is_none());
        assert!(latest_unprocessed_votes
            .update_latest_vote(vote_b, false /* should replenish */)
            .is_none());
        assert_eq!(2, latest_unprocessed_votes.len());

        assert_eq!(
            Some(1),
            latest_unprocessed_votes.get_latest_vote_slot(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(9),
            latest_unprocessed_votes.get_latest_vote_slot(keypair_b.vote_keypair.pubkey())
        );

        let vote_a = from_slots(
            vec![(0, 5), (1, 4), (3, 3), (10, 1)],
            VoteSource::Gossip,
            &keypair_a,
            None,
        );
        let vote_b = from_slots(
            vec![(0, 5), (4, 2), (6, 1)],
            VoteSource::Gossip,
            &keypair_b,
            None,
        );

        // Evict previous vote
        assert_eq!(
            1,
            latest_unprocessed_votes
                .update_latest_vote(vote_a, false /* should replenish */)
                .unwrap()
                .slot
        );
        // Drop current vote
        assert_eq!(
            6,
            latest_unprocessed_votes
                .update_latest_vote(vote_b, false /* should replenish */)
                .unwrap()
                .slot
        );

        assert_eq!(2, latest_unprocessed_votes.len());

        // Same votes should be no-ops
        let vote_a = from_slots(
            vec![(0, 5), (1, 4), (3, 3), (10, 1)],
            VoteSource::Gossip,
            &keypair_a,
            None,
        );
        let vote_b = from_slots(
            vec![(0, 5), (4, 2), (9, 1)],
            VoteSource::Gossip,
            &keypair_b,
            None,
        );
        latest_unprocessed_votes.update_latest_vote(vote_a, false /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_b, false /* should replenish */);

        assert_eq!(2, latest_unprocessed_votes.len());
        assert_eq!(
            10,
            latest_unprocessed_votes
                .get_latest_vote_slot(keypair_a.vote_keypair.pubkey())
                .unwrap()
        );
        assert_eq!(
            9,
            latest_unprocessed_votes
                .get_latest_vote_slot(keypair_b.vote_keypair.pubkey())
                .unwrap()
        );

        // Same votes with timestamps should override
        let vote_a = from_slots(
            vec![(0, 5), (1, 4), (3, 3), (10, 1)],
            VoteSource::Gossip,
            &keypair_a,
            Some(1),
        );
        let vote_b = from_slots(
            vec![(0, 5), (4, 2), (9, 1)],
            VoteSource::Gossip,
            &keypair_b,
            Some(2),
        );
        latest_unprocessed_votes.update_latest_vote(vote_a, false /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_b, false /* should replenish */);

        assert_eq!(2, latest_unprocessed_votes.len());
        assert_eq!(
            Some(1),
            latest_unprocessed_votes.get_latest_timestamp(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(2),
            latest_unprocessed_votes.get_latest_timestamp(keypair_b.vote_keypair.pubkey())
        );

        // Same votes with bigger timestamps should override
        let vote_a = from_slots(
            vec![(0, 5), (1, 4), (3, 3), (10, 1)],
            VoteSource::Gossip,
            &keypair_a,
            Some(5),
        );
        let vote_b = from_slots(
            vec![(0, 5), (4, 2), (9, 1)],
            VoteSource::Gossip,
            &keypair_b,
            Some(6),
        );
        latest_unprocessed_votes.update_latest_vote(vote_a, false /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_b, false /* should replenish */);

        assert_eq!(2, latest_unprocessed_votes.len());
        assert_eq!(
            Some(5),
            latest_unprocessed_votes.get_latest_timestamp(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(6),
            latest_unprocessed_votes.get_latest_timestamp(keypair_b.vote_keypair.pubkey())
        );

        // Same votes with smaller timestamps should not override
        let vote_a = from_slots(
            vec![(0, 5), (1, 4), (3, 3), (10, 1)],
            VoteSource::Gossip,
            &keypair_a,
            Some(2),
        );
        let vote_b = from_slots(
            vec![(0, 5), (4, 2), (9, 1)],
            VoteSource::Gossip,
            &keypair_b,
            Some(3),
        );
        latest_unprocessed_votes
            .update_latest_vote(vote_a.clone(), false /* should replenish */);
        latest_unprocessed_votes
            .update_latest_vote(vote_b.clone(), false /* should replenish */);

        assert_eq!(2, latest_unprocessed_votes.len());
        assert_eq!(
            Some(5),
            latest_unprocessed_votes.get_latest_timestamp(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(6),
            latest_unprocessed_votes.get_latest_timestamp(keypair_b.vote_keypair.pubkey())
        );

        // Drain all latest votes
        for packet in latest_unprocessed_votes
            .latest_vote_per_vote_pubkey
            .values_mut()
        {
            packet.take_vote().inspect(|_vote| {
                latest_unprocessed_votes.num_unprocessed_votes -= 1;
            });
        }
        assert_eq!(0, latest_unprocessed_votes.len());

        // Same votes with same timestamps should not replenish without flag
        latest_unprocessed_votes
            .update_latest_vote(vote_a.clone(), false /* should replenish */);
        latest_unprocessed_votes
            .update_latest_vote(vote_b.clone(), false /* should replenish */);
        assert_eq!(0, latest_unprocessed_votes.len());

        // Same votes with same timestamps should replenish with the flag
        latest_unprocessed_votes.update_latest_vote(vote_a, true /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_b, true /* should replenish */);
        assert_eq!(0, latest_unprocessed_votes.len());
    }

    #[test]
    fn test_clear() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let keypair_b = ValidatorVoteKeypairs::new_rand();
        let keypair_c = ValidatorVoteKeypairs::new_rand();
        let keypair_d = ValidatorVoteKeypairs::new_rand();
        let mut latest_unprocessed_votes = LatestUnprocessedVotes::new_for_tests(&[
            keypair_a.vote_keypair.pubkey(),
            keypair_b.vote_keypair.pubkey(),
            keypair_c.vote_keypair.pubkey(),
            keypair_d.vote_keypair.pubkey(),
        ]);

        let vote_a = from_slots(vec![(1, 1)], VoteSource::Gossip, &keypair_a, None);
        let vote_b = from_slots(vec![(2, 1)], VoteSource::Tpu, &keypair_b, None);
        let vote_c = from_slots(vec![(3, 1)], VoteSource::Tpu, &keypair_c, None);
        let vote_d = from_slots(vec![(4, 1)], VoteSource::Gossip, &keypair_d, None);

        latest_unprocessed_votes.update_latest_vote(vote_a, false /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_b, false /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_c, false /* should replenish */);
        latest_unprocessed_votes.update_latest_vote(vote_d, false /* should replenish */);
        assert_eq!(4, latest_unprocessed_votes.len());

        latest_unprocessed_votes.clear();
        assert_eq!(0, latest_unprocessed_votes.len());

        assert_eq!(
            Some(1),
            latest_unprocessed_votes.get_latest_vote_slot(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(2),
            latest_unprocessed_votes.get_latest_vote_slot(keypair_b.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(3),
            latest_unprocessed_votes.get_latest_vote_slot(keypair_c.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(4),
            latest_unprocessed_votes.get_latest_vote_slot(keypair_d.vote_keypair.pubkey())
        );
    }

    #[test]
    fn test_insert_batch_unstaked() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let keypair_b = ValidatorVoteKeypairs::new_rand();
        let keypair_c = ValidatorVoteKeypairs::new_rand();
        let keypair_d = ValidatorVoteKeypairs::new_rand();

        let vote_a = from_slots(vec![(1, 1)], VoteSource::Gossip, &keypair_a, None);
        let vote_b = from_slots(vec![(2, 1)], VoteSource::Tpu, &keypair_b, None);
        let vote_c = from_slots(vec![(3, 1)], VoteSource::Tpu, &keypair_c, None);
        let vote_d = from_slots(vec![(4, 1)], VoteSource::Gossip, &keypair_d, None);
        let votes = [
            vote_a.clone(),
            vote_b.clone(),
            vote_c.clone(),
            vote_d.clone(),
        ]
        .into_iter();

        let bank_0 = Bank::new_for_tests(&GenesisConfig::default());
        let mut latest_unprocessed_votes = LatestUnprocessedVotes::new(&bank_0);

        // Insert batch should filter out all votes as they are unstaked
        latest_unprocessed_votes.insert_batch(votes.clone(), true);
        assert!(latest_unprocessed_votes.is_empty());

        // Bank in same epoch should not update stakes
        let config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[&keypair_a], vec![200])
                .genesis_config;
        let bank_0 = Bank::new_for_tests(&config);
        let bank = Bank::new_from_parent(
            Arc::new(bank_0),
            &Pubkey::new_unique(),
            MINIMUM_SLOTS_PER_EPOCH - 1,
        );
        assert_eq!(bank.epoch(), 0);
        latest_unprocessed_votes.cache_epoch_boundary_info(&bank);
        latest_unprocessed_votes.insert_batch(votes.clone(), true);
        assert!(latest_unprocessed_votes.is_empty());

        // Bank in next epoch should update stakes
        let config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[&keypair_b], vec![200])
                .genesis_config;
        let bank_0 = Bank::new_for_tests(&config);
        let bank = Bank::new_from_parent(
            Arc::new(bank_0),
            &Pubkey::new_unique(),
            MINIMUM_SLOTS_PER_EPOCH,
        );
        assert_eq!(bank.epoch(), 1);
        latest_unprocessed_votes.cache_epoch_boundary_info(&bank);
        latest_unprocessed_votes.insert_batch(votes.clone(), true);
        assert_eq!(latest_unprocessed_votes.len(), 1);
        assert_eq!(
            latest_unprocessed_votes.get_latest_vote_slot(keypair_b.vote_keypair.pubkey()),
            Some(vote_b.slot())
        );

        // Previously unstaked votes are removed
        let config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[&keypair_c], vec![200])
                .genesis_config;
        let bank_0 = Bank::new_for_tests(&config);
        let bank = Bank::new_from_parent(
            Arc::new(bank_0),
            &Pubkey::new_unique(),
            3 * MINIMUM_SLOTS_PER_EPOCH,
        );
        assert_eq!(bank.epoch(), 2);
        latest_unprocessed_votes.cache_epoch_boundary_info(&bank);
        assert_eq!(latest_unprocessed_votes.len(), 0);
        latest_unprocessed_votes.insert_batch(votes.clone(), true);
        assert_eq!(latest_unprocessed_votes.len(), 1);
        assert_eq!(
            latest_unprocessed_votes.get_latest_vote_slot(keypair_c.vote_keypair.pubkey()),
            Some(vote_c.slot())
        );
    }
}
