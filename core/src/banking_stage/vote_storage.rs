use {
    super::latest_validator_vote_packet::{LatestValidatorVote, VoteSource},
    crate::banking_stage::transaction_scheduler::transaction_state_container::SharedBytes,
    agave_feature_set as feature_set,
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    ahash::HashMap,
    itertools::Itertools,
    rand::{rng, Rng},
    solana_account::from_account,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        epoch_stakes::{EpochAuthorizedVoters, VersionedEpochStakes},
    },
    solana_sysvar::{self as sysvar, slot_hashes::SlotHashes},
    std::{cmp, sync::Arc},
};

/// Maximum number of votes a single receive call will accept
const MAX_NUM_VOTES_RECEIVE: usize = 10_000;

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
pub struct VoteStorage {
    latest_vote_per_vote_pubkey: HashMap<Pubkey, LatestValidatorVote>,
    num_unprocessed_votes: usize,
    cached_epoch_stakes: VersionedEpochStakes,
    /// Authorized voters for the current epoch. This is separate from
    /// cached_epoch_stakes because stakes are offset by one epoch (we use
    /// epoch E + 1 for stake in epoch E), but authorized voters must
    /// match the epoch of the bank slot
    cached_epoch_authorized_voters: Arc<EpochAuthorizedVoters>,
    deprecate_legacy_vote_ixs: bool,
    current_epoch: Epoch,
}

impl VoteStorage {
    pub fn new(bank: &Bank) -> Self {
        let cached_epoch_stakes = bank.current_epoch_stakes().clone();
        let cached_epoch_authorized_voters = bank
            .epoch_stakes(bank.epoch())
            .expect("Current epoch stakes must exist")
            .epoch_authorized_voters()
            .clone();
        Self {
            latest_vote_per_vote_pubkey: HashMap::default(),
            num_unprocessed_votes: 0,
            cached_epoch_stakes,
            cached_epoch_authorized_voters,
            current_epoch: bank.epoch(),
            deprecate_legacy_vote_ixs: bank
                .feature_set
                .is_active(&feature_set::deprecate_legacy_vote_ixs::id()),
        }
    }

    #[cfg(test)]
    pub fn new_for_tests(vote_pubkeys_to_stake: &[Pubkey]) -> Self {
        use solana_vote::vote_account::VoteAccount;

        let vote_accounts = vote_pubkeys_to_stake
            .iter()
            .map(|pubkey| (*pubkey, (1u64, VoteAccount::new_random())))
            .collect();
        let epoch_stakes = VersionedEpochStakes::new_for_tests(vote_accounts, 0);
        // Authorized voters don't change in tests so it's fine to use the authorized voters from the "wrong" epoch
        let epoch_authorized_voters = epoch_stakes.epoch_authorized_voters().clone();

        Self {
            latest_vote_per_vote_pubkey: HashMap::default(),
            num_unprocessed_votes: 0,
            cached_epoch_stakes: epoch_stakes,
            cached_epoch_authorized_voters: epoch_authorized_voters,
            current_epoch: 0,
            deprecate_legacy_vote_ixs: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.num_unprocessed_votes
    }

    pub fn max_receive_size(&self) -> usize {
        MAX_NUM_VOTES_RECEIVE
    }

    pub(crate) fn insert_batch(
        &mut self,
        vote_source: VoteSource,
        votes: impl Iterator<Item = SanitizedTransactionView<SharedBytes>>,
    ) -> VoteBatchInsertionMetrics {
        let should_deprecate_legacy_vote_ixs = self.deprecate_legacy_vote_ixs;
        self.insert_batch_with_replenish(
            votes.filter_map(|vote| {
                LatestValidatorVote::new_from_view(
                    vote,
                    vote_source,
                    should_deprecate_legacy_vote_ixs,
                )
                .ok()
            }),
            false,
        )
    }

    // Re-insert re-tryable packets.
    pub(crate) fn reinsert_packets(
        &mut self,
        packets: impl Iterator<Item = SanitizedTransactionView<SharedBytes>>,
    ) {
        let should_deprecate_legacy_vote_ixs = self.deprecate_legacy_vote_ixs;
        self.insert_batch_with_replenish(
            packets.filter_map(|packet| {
                LatestValidatorVote::new_from_view(
                    packet,
                    VoteSource::Tpu, // incorrect, but this bug has been here w/o issue for a long time.
                    should_deprecate_legacy_vote_ixs,
                )
                .ok()
            }),
            true,
        );
    }

    pub fn drain_unprocessed(&mut self, bank: &Bank) -> Vec<SanitizedTransactionView<SharedBytes>> {
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

    pub fn clear(&mut self) {
        self.latest_vote_per_vote_pubkey
            .values_mut()
            .for_each(|vote| {
                if vote.take_vote().is_some() {
                    self.num_unprocessed_votes -= 1;
                }
            });
    }

    pub fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        if bank.epoch() <= self.current_epoch {
            return;
        }
        {
            // Stakes are offset by one epoch
            let current_epoch_stakes = bank.current_epoch_stakes().clone();
            // Authorized voters use the same epoch as the leader bank
            self.cached_epoch_authorized_voters = bank
                .epoch_stakes(bank.epoch())
                .map(|stakes| stakes.epoch_authorized_voters().clone())
                // Should be fine to expect as the current epoch must exist in epoch_stakes,
                // will cleanup in a follow up
                .unwrap_or_else(|| current_epoch_stakes.epoch_authorized_voters().clone());
            self.cached_epoch_stakes = current_epoch_stakes;
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

    fn insert_batch_with_replenish(
        &mut self,
        votes: impl Iterator<Item = LatestValidatorVote>,
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

            if self
                .cached_epoch_authorized_voters
                .get(&vote.vote_pubkey())
                .is_none_or(|authorized| authorized != &vote.authorized_voter_pubkey())
            {
                continue;
            }

            if let Some(vote) = self.update_latest_vote(vote, should_replenish_taken_votes) {
                match vote.source() {
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
    fn update_latest_vote(
        &mut self,
        vote: LatestValidatorVote,
        should_replenish_taken_votes: bool,
    ) -> Option<LatestValidatorVote> {
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

    /// Allow votes for later slots or the same slot with later timestamp (refreshed votes)
    /// We directly compare as options to prioritize votes for same slot with timestamp as
    /// Some > None
    fn allow_update(
        vote: &LatestValidatorVote,
        latest_vote: &LatestValidatorVote,
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

    fn weighted_random_order_by_stake(&self) -> impl Iterator<Item = Pubkey> + use<> {
        // Efraimidis and Spirakis algo for weighted random sample without replacement
        let mut pubkey_with_weight: Vec<(f64, Pubkey)> = self
            .latest_vote_per_vote_pubkey
            .keys()
            .filter_map(|&pubkey| {
                let stake = self.cached_epoch_stakes.vote_account_stake(&pubkey);
                if stake == 0 {
                    None // Ignore votes from unstaked validators
                } else {
                    Some((rng().random::<f64>().powf(1.0 / (stake as f64)), pubkey))
                }
            })
            .collect::<Vec<_>>();
        pubkey_with_weight.sort_by(|(w1, _), (w2, _)| w2.partial_cmp(w1).unwrap());
        pubkey_with_weight.into_iter().map(|(_, pubkey)| pubkey)
    }

    /// Check if `vote` can land in our fork based on `slot_hashes`
    fn is_valid_for_our_fork(vote: &LatestValidatorVote, slot_hashes: &Option<SlotHashes>) -> bool {
        let Some(slot_hashes) = slot_hashes else {
            // When slot hashes is not present we do not filter
            return true;
        };
        slot_hashes
            .get(&vote.slot())
            .map(|found_hash| *found_hash == vote.hash())
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn get_latest_vote_slot(&self, pubkey: Pubkey) -> Option<solana_clock::Slot> {
        self.latest_vote_per_vote_pubkey
            .get(&pubkey)
            .map(|l| l.slot())
    }

    #[cfg(test)]
    fn get_latest_timestamp(&self, pubkey: Pubkey) -> Option<solana_clock::UnixTimestamp> {
        self.latest_vote_per_vote_pubkey
            .get(&pubkey)
            .and_then(|l| l.timestamp())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        solana_clock::UnixTimestamp,
        solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_perf::packet::{BytesPacket, PacketFlags},
        solana_runtime::genesis_utils::{self, ValidatorVoteKeypairs},
        solana_signer::Signer,
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::sync::Arc,
    };

    /// Create a VoteAccount with a specific authorized voter for the given epoch
    fn vote_account_with_authorized_voter(
        vote_pubkey: &Pubkey,
        authorized_voter: &Pubkey,
        epoch: solana_clock::Epoch,
    ) -> solana_vote::vote_account::VoteAccount {
        use {
            solana_account::AccountSharedData,
            solana_vote_program::vote_state::{VoteInit, VoteStateV4, VoteStateVersions},
        };

        let vote_init = VoteInit {
            node_pubkey: Pubkey::new_unique(),
            authorized_voter: *authorized_voter,
            authorized_withdrawer: Pubkey::new_unique(),
            commission: 0,
        };
        let clock = solana_clock::Clock {
            slot: 0,
            epoch_start_timestamp: 0,
            epoch,
            leader_schedule_epoch: epoch,
            unix_timestamp: 0,
        };
        let vote_state = VoteStateV4::new_with_defaults(vote_pubkey, &vote_init, &clock);
        let account = AccountSharedData::new_data(
            1_000_000,
            &VoteStateVersions::new_v4(vote_state),
            &solana_sdk_ids::vote::id(),
        )
        .unwrap();

        solana_vote::vote_account::VoteAccount::try_from(account).unwrap()
    }

    pub(crate) fn packet_from_slots(
        slots: Vec<(u64, u32)>,
        keypairs: &ValidatorVoteKeypairs,
        timestamp: Option<UnixTimestamp>,
    ) -> BytesPacket {
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
        let mut packet = BytesPacket::from_data(None, vote_tx).unwrap();
        packet
            .meta_mut()
            .flags
            .set(PacketFlags::SIMPLE_VOTE_TX, true);

        packet
    }

    fn from_slots(
        slots: Vec<(u64, u32)>,
        vote_source: VoteSource,
        keypairs: &ValidatorVoteKeypairs,
        timestamp: Option<UnixTimestamp>,
    ) -> LatestValidatorVote {
        let packet = packet_from_slots(slots, keypairs, timestamp);
        LatestValidatorVote::new(packet.as_ref(), vote_source, true).unwrap()
    }

    /// Create a vote packet with a custom authorized voter keypair
    fn packet_from_slots_with_authorized_voter(
        slots: Vec<(u64, u32)>,
        keypairs: &ValidatorVoteKeypairs,
        authorized_voter: &Keypair,
        timestamp: Option<UnixTimestamp>,
    ) -> BytesPacket {
        let mut vote = TowerSync::from(slots);
        vote.timestamp = timestamp;
        let vote_tx = new_tower_sync_transaction(
            vote,
            Hash::new_unique(),
            &keypairs.node_keypair,
            &keypairs.vote_keypair,
            authorized_voter,
            None,
        );
        let mut packet = BytesPacket::from_data(None, vote_tx).unwrap();
        packet
            .meta_mut()
            .flags
            .set(PacketFlags::SIMPLE_VOTE_TX, true);

        packet
    }

    fn to_sanitized_view(packet: BytesPacket) -> SanitizedTransactionView<SharedBytes> {
        SanitizedTransactionView::try_new_sanitized(Arc::new(packet.buffer().to_vec()), false, true)
            .unwrap()
    }

    #[test]
    fn test_reinsert_packets() {
        let keypair = ValidatorVoteKeypairs::new_rand();
        let genesis_config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[&keypair], vec![200])
                .genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let vote = packet_from_slots(vec![(0, 1)], &keypair, None);
        let mut vote_storage = VoteStorage::new(&bank);
        vote_storage.insert_batch(VoteSource::Tpu, std::iter::once(to_sanitized_view(vote)));
        assert_eq!(1, vote_storage.len());

        // Drain all packets, then re-insert.
        let packets = vote_storage.drain_unprocessed(&bank);
        vote_storage.reinsert_packets(packets.into_iter());

        // All packets should remain in the transaction storage
        assert_eq!(1, vote_storage.len());
    }

    #[test]
    fn test_update_latest_vote() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let keypair_b = ValidatorVoteKeypairs::new_rand();
        let mut vote_storage = VoteStorage::new_for_tests(&[
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

        assert!(vote_storage
            .update_latest_vote(vote_a, false /* should replenish */)
            .is_none());
        assert!(vote_storage
            .update_latest_vote(vote_b, false /* should replenish */)
            .is_none());
        assert_eq!(2, vote_storage.len());

        assert_eq!(
            Some(1),
            vote_storage.get_latest_vote_slot(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(9),
            vote_storage.get_latest_vote_slot(keypair_b.vote_keypair.pubkey())
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
            vote_storage
                .update_latest_vote(vote_a, false /* should replenish */)
                .unwrap()
                .slot()
        );
        // Drop current vote
        assert_eq!(
            6,
            vote_storage
                .update_latest_vote(vote_b, false /* should replenish */)
                .unwrap()
                .slot()
        );

        assert_eq!(2, vote_storage.len());

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
        vote_storage.update_latest_vote(vote_a, false /* should replenish */);
        vote_storage.update_latest_vote(vote_b, false /* should replenish */);

        assert_eq!(2, vote_storage.len());
        assert_eq!(
            10,
            vote_storage
                .get_latest_vote_slot(keypair_a.vote_keypair.pubkey())
                .unwrap()
        );
        assert_eq!(
            9,
            vote_storage
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
        vote_storage.update_latest_vote(vote_a, false /* should replenish */);
        vote_storage.update_latest_vote(vote_b, false /* should replenish */);

        assert_eq!(2, vote_storage.len());
        assert_eq!(
            Some(1),
            vote_storage.get_latest_timestamp(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(2),
            vote_storage.get_latest_timestamp(keypair_b.vote_keypair.pubkey())
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
        vote_storage.update_latest_vote(vote_a, false /* should replenish */);
        vote_storage.update_latest_vote(vote_b, false /* should replenish */);

        assert_eq!(2, vote_storage.len());
        assert_eq!(
            Some(5),
            vote_storage.get_latest_timestamp(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(6),
            vote_storage.get_latest_timestamp(keypair_b.vote_keypair.pubkey())
        );

        // Same votes with smaller timestamps should not override
        let vote_a = || {
            from_slots(
                vec![(0, 5), (1, 4), (3, 3), (10, 1)],
                VoteSource::Gossip,
                &keypair_a,
                Some(2),
            )
        };
        let vote_b = || {
            from_slots(
                vec![(0, 5), (4, 2), (9, 1)],
                VoteSource::Gossip,
                &keypair_b,
                Some(3),
            )
        };
        vote_storage.update_latest_vote(vote_a(), false /* should replenish */);
        vote_storage.update_latest_vote(vote_b(), false /* should replenish */);

        assert_eq!(2, vote_storage.len());
        assert_eq!(
            Some(5),
            vote_storage.get_latest_timestamp(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(6),
            vote_storage.get_latest_timestamp(keypair_b.vote_keypair.pubkey())
        );

        // Drain all latest votes
        for packet in vote_storage.latest_vote_per_vote_pubkey.values_mut() {
            packet.take_vote().inspect(|_vote| {
                vote_storage.num_unprocessed_votes -= 1;
            });
        }
        assert_eq!(0, vote_storage.len());

        // Same votes with same timestamps should not replenish without flag
        vote_storage.update_latest_vote(vote_a(), false /* should replenish */);
        vote_storage.update_latest_vote(vote_b(), false /* should replenish */);
        assert_eq!(0, vote_storage.len());

        // Same votes with same timestamps should replenish with the flag
        vote_storage.update_latest_vote(vote_a(), true /* should replenish */);
        vote_storage.update_latest_vote(vote_b(), true /* should replenish */);
        assert_eq!(0, vote_storage.len());
    }

    #[test]
    fn test_clear() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let keypair_b = ValidatorVoteKeypairs::new_rand();
        let keypair_c = ValidatorVoteKeypairs::new_rand();
        let keypair_d = ValidatorVoteKeypairs::new_rand();
        let mut vote_storage = VoteStorage::new_for_tests(&[
            keypair_a.vote_keypair.pubkey(),
            keypair_b.vote_keypair.pubkey(),
            keypair_c.vote_keypair.pubkey(),
            keypair_d.vote_keypair.pubkey(),
        ]);

        let vote_a = from_slots(vec![(1, 1)], VoteSource::Gossip, &keypair_a, None);
        let vote_b = from_slots(vec![(2, 1)], VoteSource::Tpu, &keypair_b, None);
        let vote_c = from_slots(vec![(3, 1)], VoteSource::Tpu, &keypair_c, None);
        let vote_d = from_slots(vec![(4, 1)], VoteSource::Gossip, &keypair_d, None);

        vote_storage.update_latest_vote(vote_a, false /* should replenish */);
        vote_storage.update_latest_vote(vote_b, false /* should replenish */);
        vote_storage.update_latest_vote(vote_c, false /* should replenish */);
        vote_storage.update_latest_vote(vote_d, false /* should replenish */);
        assert_eq!(4, vote_storage.len());

        vote_storage.clear();
        assert_eq!(0, vote_storage.len());

        assert_eq!(
            Some(1),
            vote_storage.get_latest_vote_slot(keypair_a.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(2),
            vote_storage.get_latest_vote_slot(keypair_b.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(3),
            vote_storage.get_latest_vote_slot(keypair_c.vote_keypair.pubkey())
        );
        assert_eq!(
            Some(4),
            vote_storage.get_latest_vote_slot(keypair_d.vote_keypair.pubkey())
        );
    }

    #[test]
    fn test_insert_batch_authorized_voter() {
        // Test that votes are only accepted when signed by the correct authorized voter.
        let keypair = ValidatorVoteKeypairs::new_rand();
        let unauthorized_keypair = solana_keypair::Keypair::new();

        let genesis_config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[&keypair], vec![200])
                .genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut vote_storage = VoteStorage::new(&bank);

        // Vote signed by the correct authorized voter (vote_keypair) should be accepted
        let correct_vote = packet_from_slots_with_authorized_voter(
            vec![(0, 1)],
            &keypair,
            &keypair.vote_keypair,
            None,
        );
        vote_storage.insert_batch(
            VoteSource::Tpu,
            std::iter::once(to_sanitized_view(correct_vote)),
        );
        assert_eq!(1, vote_storage.len());
        assert_eq!(
            Some(0),
            vote_storage.get_latest_vote_slot(keypair.vote_keypair.pubkey())
        );

        // Vote signed by an unauthorized keypair should be filtered out
        let unauthorized_vote = packet_from_slots_with_authorized_voter(
            vec![(1, 1)],
            &keypair,
            &unauthorized_keypair,
            None,
        );
        vote_storage.insert_batch(
            VoteSource::Tpu,
            std::iter::once(to_sanitized_view(unauthorized_vote)),
        );
        // Should still be 1 (unauthorized vote was filtered)
        assert_eq!(1, vote_storage.len());
        // Slot should still be 0 (the authorized vote), not 1 (the unauthorized one)
        assert_eq!(
            Some(0),
            vote_storage.get_latest_vote_slot(keypair.vote_keypair.pubkey())
        );

        // Update with a valid vote for a later slot - should succeed
        let correct_vote_2 = packet_from_slots_with_authorized_voter(
            vec![(2, 1)],
            &keypair,
            &keypair.vote_keypair,
            None,
        );
        vote_storage.insert_batch(
            VoteSource::Tpu,
            std::iter::once(to_sanitized_view(correct_vote_2)),
        );
        assert_eq!(1, vote_storage.len());
        assert_eq!(
            Some(2),
            vote_storage.get_latest_vote_slot(keypair.vote_keypair.pubkey())
        );
    }

    /// Test that authorized voters are checked against the current epoch, not the next epoch.
    /// This verifies that cache_epoch_boundary_info uses epoch_stakes(bank.epoch()) for
    /// authorized voters, not current_epoch_stakes() which returns epoch E+1.
    #[test]
    fn test_authorized_voter_uses_current_epoch_not_next() {
        let keypair = ValidatorVoteKeypairs::new_rand();
        let vote_pubkey = keypair.vote_keypair.pubkey();

        // Create two different authorized voters: one for epoch 1, one for epoch 2
        let epoch1_authorized_voter_keypair = keypair.node_keypair.insecure_clone();
        let epoch1_authorized_voter = epoch1_authorized_voter_keypair.pubkey();
        let epoch2_authorized_voter_keypair = Keypair::new();
        let epoch2_authorized_voter = epoch2_authorized_voter_keypair.pubkey();

        // Create vote accounts with different authorized voters for different epochs
        let vote_account_epoch1 =
            vote_account_with_authorized_voter(&vote_pubkey, &epoch1_authorized_voter, 1);
        let vote_account_epoch2 =
            vote_account_with_authorized_voter(&vote_pubkey, &epoch2_authorized_voter, 2);

        // Create epoch stakes for epochs 1 and 2
        let epoch1_stakes = VersionedEpochStakes::new_for_tests(
            [(vote_pubkey, (100, vote_account_epoch1))]
                .into_iter()
                .collect(),
            1, // leader_schedule_epoch
        );
        let epoch2_stakes = VersionedEpochStakes::new_for_tests(
            [(vote_pubkey, (100, vote_account_epoch2))]
                .into_iter()
                .collect(),
            2, // leader_schedule_epoch
        );

        // Create a bank in epoch 1 with custom epoch stakes
        let genesis_config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[&keypair], vec![200])
                .genesis_config;
        let bank_0 = Bank::new_for_tests(&genesis_config);
        let mut bank = Bank::new_from_parent(
            Arc::new(bank_0),
            &Pubkey::new_unique(),
            MINIMUM_SLOTS_PER_EPOCH, // This puts us in epoch 1
        );
        assert_eq!(bank.epoch(), 1);

        // Set custom epoch stakes: epoch 1 has epoch1_authorized_voter, epoch 2 has epoch2_authorized_voter
        bank.set_epoch_stakes_for_test(1, epoch1_stakes);
        bank.set_epoch_stakes_for_test(2, epoch2_stakes);

        let mut vote_storage = VoteStorage::new(&bank);

        // Vote signed by epoch 1's authorized voter (vote_keypair) should be accepted
        let epoch1_vote = packet_from_slots_with_authorized_voter(
            vec![(MINIMUM_SLOTS_PER_EPOCH, 1)],
            &keypair,
            &epoch1_authorized_voter_keypair,
            None,
        );
        vote_storage.insert_batch(
            VoteSource::Tpu,
            std::iter::once(to_sanitized_view(epoch1_vote)),
        );
        assert_eq!(
            1,
            vote_storage.len(),
            "Vote with epoch 1 authorized voter should be accepted"
        );

        // Vote signed by epoch 2's authorized voter should be REJECTED
        // If we were incorrectly using current_epoch_stakes() (epoch 2), this would be accepted
        let wrong_epoch_vote = packet_from_slots_with_authorized_voter(
            vec![(MINIMUM_SLOTS_PER_EPOCH + 1, 1)],
            &keypair,
            &epoch2_authorized_voter_keypair, // This won't match epoch 1's authorized voter
            None,
        );
        vote_storage.insert_batch(
            VoteSource::Tpu,
            std::iter::once(to_sanitized_view(wrong_epoch_vote)),
        );
        // Should still be 1 - the vote with wrong authorized voter was rejected
        assert_eq!(
            1,
            vote_storage.len(),
            "Vote with wrong authorized voter should be rejected"
        );
    }

    #[test]
    fn test_insert_batch_unstaked() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let keypair_b = ValidatorVoteKeypairs::new_rand();
        let keypair_c = ValidatorVoteKeypairs::new_rand();
        let keypair_d = ValidatorVoteKeypairs::new_rand();

        let vote_b_slot = 2;
        let vote_c_slot = 3;
        let vote_a = packet_from_slots(vec![(1, 1)], &keypair_a, None);
        let vote_b = packet_from_slots(vec![(vote_b_slot, 1)], &keypair_b, None);
        let vote_c = packet_from_slots(vec![(vote_c_slot, 1)], &keypair_c, None);
        let vote_d = packet_from_slots(vec![(4, 1)], &keypair_d, None);
        let votes = || {
            vec![
                to_sanitized_view(vote_a.clone()),
                to_sanitized_view(vote_b.clone()),
                to_sanitized_view(vote_c.clone()),
                to_sanitized_view(vote_d.clone()),
            ]
        };

        let bank_0 = Bank::new_for_tests(&GenesisConfig::default());
        let mut vote_storage = VoteStorage::new(&bank_0);

        // Insert batch should filter out all votes as they are unstaked
        vote_storage.insert_batch(VoteSource::Tpu, votes().into_iter());
        assert!(vote_storage.is_empty());

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
        vote_storage.cache_epoch_boundary_info(&bank);
        vote_storage.insert_batch(VoteSource::Tpu, votes().into_iter());
        assert!(vote_storage.is_empty());

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
        vote_storage.cache_epoch_boundary_info(&bank);
        vote_storage.insert_batch(VoteSource::Gossip, votes().into_iter());
        assert_eq!(vote_storage.len(), 1);
        assert_eq!(
            vote_storage.get_latest_vote_slot(keypair_b.vote_keypair.pubkey()),
            Some(vote_b_slot)
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
        vote_storage.cache_epoch_boundary_info(&bank);
        assert_eq!(vote_storage.len(), 0);
        vote_storage.insert_batch(VoteSource::Tpu, votes().into_iter());
        assert_eq!(vote_storage.len(), 1);
        assert_eq!(
            vote_storage.get_latest_vote_slot(keypair_c.vote_keypair.pubkey()),
            Some(vote_c_slot)
        );
    }
}
