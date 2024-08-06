use {
    crate::solana::wen_restart_proto::LastVotedForkSlotsRecord,
    anyhow::Result,
    log::*,
    solana_gossip::restart_crds_values::RestartLastVotedForkSlots,
    solana_runtime::bank::Bank,
    solana_sdk::{
        clock::{Epoch, Slot},
        hash::Hash,
        pubkey::Pubkey,
    },
    std::{
        cmp::Ordering,
        collections::{BTreeSet, HashMap, HashSet},
        str::FromStr,
        sync::Arc,
    },
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LastVotedForkSlotsEpochInfo {
    pub epoch: Epoch,
    pub total_stake: u64,
    pub total_active_stake: u64,
}

pub(crate) struct LastVotedForkSlotsAggregate {
    active_peers: HashSet<Pubkey>,
    epoch_info_vec: Vec<LastVotedForkSlotsEpochInfo>,
    last_voted_fork_slots: HashMap<Pubkey, RestartLastVotedForkSlots>,
    my_pubkey: Pubkey,
    repair_threshold: f64,
    root_bank: Arc<Bank>,
    slots_stake_map: HashMap<Slot, u64>,
    slots_to_repair: BTreeSet<Slot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LastVotedForkSlotsFinalResult {
    pub slots_stake_map: HashMap<Slot, u64>,
    pub epoch_info_vec: Vec<LastVotedForkSlotsEpochInfo>,
}

impl LastVotedForkSlotsAggregate {
    fn validator_stake_at_epoch(bank: &Arc<Bank>, epoch: &Epoch, id: &Pubkey) -> Option<u64> {
        bank.epoch_stakes(*epoch)
            .and_then(|epoch_stakes| epoch_stakes.node_id_to_stake(id))
    }

    fn validator_stake_at_slot(bank: &Arc<Bank>, slot: &Slot, id: &Pubkey) -> Option<u64> {
        let epoch = bank.epoch_schedule().get_epoch(*slot);
        Self::validator_stake_at_epoch(bank, &epoch, id)
    }

    fn total_stake_at_slot(bank: &Arc<Bank>, slot: &Slot) -> Option<u64> {
        let epoch = bank.epoch_schedule().get_epoch(*slot);
        bank.epoch_total_stake(epoch)
    }

    pub(crate) fn new(
        root_bank: Arc<Bank>,
        repair_threshold: f64,
        last_voted_fork_slots: &Vec<Slot>,
        my_pubkey: &Pubkey,
    ) -> Self {
        let mut active_peers = HashSet::new();
        active_peers.insert(*my_pubkey);
        let mut slots_stake_map = HashMap::new();
        let root_slot = root_bank.slot();
        let root_epoch = root_bank.epoch();
        for slot in last_voted_fork_slots {
            if slot >= &root_slot {
                if let Some(sender_stake) =
                    Self::validator_stake_at_slot(&root_bank, slot, my_pubkey)
                {
                    slots_stake_map.insert(*slot, sender_stake);
                } else {
                    warn!(
                        "The root bank {} does not have the stake for slot {}",
                        root_slot, slot
                    );
                }
            }
        }
        let epoch_info_vec = vec![LastVotedForkSlotsEpochInfo {
            epoch: root_epoch,
            total_stake: Self::total_stake_at_slot(&root_bank, &root_slot).unwrap_or(0),
            total_active_stake: 0,
        }];
        Self {
            active_peers,
            epoch_info_vec,
            last_voted_fork_slots: HashMap::new(),
            my_pubkey: *my_pubkey,
            repair_threshold,
            root_bank,
            slots_stake_map,
            slots_to_repair: BTreeSet::new(),
        }
    }

    pub(crate) fn aggregate_from_record(
        &mut self,
        key_string: &str,
        record: &LastVotedForkSlotsRecord,
    ) -> Result<Option<LastVotedForkSlotsRecord>> {
        let from = Pubkey::from_str(key_string)?;
        if from == self.my_pubkey {
            return Ok(None);
        }
        let last_voted_hash = Hash::from_str(&record.last_vote_bankhash)?;
        let converted_record = RestartLastVotedForkSlots::new(
            from,
            record.wallclock,
            &record.last_voted_fork_slots,
            last_voted_hash,
            record.shred_version as u16,
        )?;
        Ok(self.aggregate(converted_record))
    }

    pub(crate) fn aggregate(
        &mut self,
        new_slots: RestartLastVotedForkSlots,
    ) -> Option<LastVotedForkSlotsRecord> {
        let from = &new_slots.from;
        if from == &self.my_pubkey {
            return None;
        }
        self.active_peers.insert(*from);
        let root_slot = self.root_bank.slot();
        let new_slots_vec = new_slots.to_slots(root_slot);
        let record = LastVotedForkSlotsRecord {
            last_voted_fork_slots: new_slots_vec.clone(),
            last_vote_bankhash: new_slots.last_voted_hash.to_string(),
            shred_version: new_slots.shred_version as u32,
            wallclock: new_slots.wallclock,
        };
        if self.update_and_check_if_message_already_saved(new_slots, new_slots_vec) {
            return None;
        }
        self.update_epoch_info_vec();
        Some(record)
    }

    // Return true if the message has already been saved, so we can skip the rest of the processing.
    fn update_and_check_if_message_already_saved(
        &mut self,
        new_slots: RestartLastVotedForkSlots,
        new_slots_vec: Vec<Slot>,
    ) -> bool {
        let from = &new_slots.from;
        let new_slots_set: HashSet<Slot> = HashSet::from_iter(new_slots_vec);
        let old_slots_set = match self.last_voted_fork_slots.insert(*from, new_slots.clone()) {
            Some(old_slots) => {
                if old_slots == new_slots {
                    return true;
                } else {
                    HashSet::from_iter(old_slots.to_slots(self.root_bank.slot()))
                }
            }
            None => HashSet::new(),
        };
        for slot in old_slots_set.difference(&new_slots_set) {
            let entry = self.slots_stake_map.get_mut(slot).unwrap();
            if let Some(sender_stake) = Self::validator_stake_at_slot(&self.root_bank, slot, from) {
                *entry = entry.saturating_sub(sender_stake);
                let repair_threshold_stake = (Self::total_stake_at_slot(&self.root_bank, slot)
                    .unwrap() as f64
                    * self.repair_threshold) as u64;
                if *entry < repair_threshold_stake {
                    self.slots_to_repair.remove(slot);
                }
            }
        }
        for slot in new_slots_set.difference(&old_slots_set) {
            let entry = self.slots_stake_map.entry(*slot).or_insert(0);
            if let Some(sender_stake) = Self::validator_stake_at_slot(&self.root_bank, slot, from) {
                *entry = entry.saturating_add(sender_stake);
                let repair_threshold_stake = (Self::total_stake_at_slot(&self.root_bank, slot)
                    .unwrap() as f64
                    * self.repair_threshold) as u64;
                if *entry >= repair_threshold_stake {
                    self.slots_to_repair.insert(*slot);
                }
            }
        }
        false
    }

    fn update_epoch_info_vec(&mut self) {
        let highest_repair_slot = self.slots_to_repair.last();
        match highest_repair_slot {
            Some(slot) => {
                let current_epoch = self.root_bank.epoch_schedule().get_epoch(*slot);
                let last_entry_epoch = self.epoch_info_vec.last().unwrap().epoch;
                match last_entry_epoch.cmp(&current_epoch) {
                    Ordering::Less => {
                        // wandering into new epoch, add to the end of state vec, we should have at most 2 entries.
                        self.epoch_info_vec.push(LastVotedForkSlotsEpochInfo {
                            epoch: current_epoch,
                            total_stake: Self::total_stake_at_slot(&self.root_bank, slot).unwrap(),
                            total_active_stake: 0,
                        });
                    }
                    Ordering::Greater => {
                        // Somehow wandering into new epoch and no more because someone changed votes, let's remove
                        // the new epoch and update the old one.
                        assert!(self.epoch_info_vec.first().unwrap().epoch == current_epoch);
                        self.epoch_info_vec.truncate(1);
                    }
                    Ordering::Equal => {}
                }
                for entry in self.epoch_info_vec.iter_mut().rev() {
                    entry.total_active_stake =
                        self.active_peers.iter().fold(0, |sum: u64, pubkey| {
                            sum.saturating_add(
                                Self::validator_stake_at_epoch(
                                    &self.root_bank,
                                    &entry.epoch,
                                    pubkey,
                                )
                                .unwrap_or(0),
                            )
                        });
                }
            }
            None => {
                self.epoch_info_vec.truncate(1);
                let first_entry = self.epoch_info_vec.first_mut().unwrap();
                first_entry.total_active_stake = 0;
            }
        }
    }

    pub(crate) fn min_active_percent(&self) -> f64 {
        self.epoch_info_vec
            .iter()
            .map(|info| info.total_active_stake as f64 / info.total_stake as f64 * 100.0)
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0)
    }

    pub(crate) fn slots_to_repair_iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots_to_repair.iter()
    }

    pub(crate) fn get_final_result(self) -> LastVotedForkSlotsFinalResult {
        LastVotedForkSlotsFinalResult {
            slots_stake_map: self.slots_stake_map,
            epoch_info_vec: self.epoch_info_vec,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            last_voted_fork_slots_aggregate::*, solana::wen_restart_proto::LastVotedForkSlotsRecord,
        },
        solana_gossip::restart_crds_values::RestartLastVotedForkSlots,
        solana_program::clock::Slot,
        solana_runtime::{
            bank::Bank,
            genesis_utils::{
                create_genesis_config_with_vote_accounts, GenesisConfigInfo, ValidatorVoteKeypairs,
            },
        },
        solana_sdk::{hash::Hash, signature::Signer, timing::timestamp},
    };

    const TOTAL_VALIDATOR_COUNT: u16 = 10;
    const MY_INDEX: usize = 9;
    const REPAIR_THRESHOLD: f64 = 0.42;
    const SHRED_VERSION: u16 = 52;

    struct TestAggregateInitResult {
        pub slots_aggregate: LastVotedForkSlotsAggregate,
        pub validator_voting_keypairs: Vec<ValidatorVoteKeypairs>,
        pub root_slot: Slot,
        pub last_voted_fork_slots: Vec<Slot>,
    }

    fn test_aggregate_init() -> TestAggregateInitResult {
        solana_logger::setup();
        let validator_voting_keypairs: Vec<_> = (0..TOTAL_VALIDATOR_COUNT)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config_with_vote_accounts(
            10_000,
            &validator_voting_keypairs,
            vec![100; validator_voting_keypairs.len()],
        );
        let (_, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let root_bank = bank_forks.read().unwrap().root_bank();
        let root_slot = root_bank.slot();
        let last_voted_fork_slots = vec![
            root_slot.saturating_add(1),
            root_slot.saturating_add(2),
            root_slot.saturating_add(3),
        ];
        TestAggregateInitResult {
            slots_aggregate: LastVotedForkSlotsAggregate::new(
                root_bank,
                REPAIR_THRESHOLD,
                &last_voted_fork_slots,
                &validator_voting_keypairs[MY_INDEX].node_keypair.pubkey(),
            ),
            validator_voting_keypairs,
            root_slot,
            last_voted_fork_slots,
        }
    }

    #[test]
    fn test_aggregate() {
        let mut test_state = test_aggregate_init();
        let root_slot = test_state.root_slot;
        // Until one slot reaches 42% stake, the percentage should be 0.
        assert_eq!(test_state.slots_aggregate.min_active_percent(), 0.0);
        let initial_num_active_validators = 3;
        for validator_voting_keypair in test_state
            .validator_voting_keypairs
            .iter()
            .take(initial_num_active_validators)
        {
            let pubkey = validator_voting_keypair.node_keypair.pubkey();
            let now = timestamp();
            assert_eq!(
                test_state.slots_aggregate.aggregate(
                    RestartLastVotedForkSlots::new(
                        pubkey,
                        now,
                        &test_state.last_voted_fork_slots,
                        Hash::default(),
                        SHRED_VERSION,
                    )
                    .unwrap(),
                ),
                Some(LastVotedForkSlotsRecord {
                    last_voted_fork_slots: test_state.last_voted_fork_slots.clone(),
                    last_vote_bankhash: Hash::default().to_string(),
                    shred_version: SHRED_VERSION as u32,
                    wallclock: now,
                }),
            );
        }
        assert_eq!(test_state.slots_aggregate.min_active_percent(), 0.0);
        assert!(test_state
            .slots_aggregate
            .slots_to_repair_iter()
            .next()
            .is_none());

        let new_active_validator = test_state.validator_voting_keypairs
            [initial_num_active_validators + 1]
            .node_keypair
            .pubkey();
        let now = timestamp();
        let new_active_validator_last_voted_slots = RestartLastVotedForkSlots::new(
            new_active_validator,
            now,
            &test_state.last_voted_fork_slots,
            Hash::default(),
            SHRED_VERSION,
        )
        .unwrap();
        assert_eq!(
            test_state
                .slots_aggregate
                .aggregate(new_active_validator_last_voted_slots),
            Some(LastVotedForkSlotsRecord {
                last_voted_fork_slots: test_state.last_voted_fork_slots.clone(),
                last_vote_bankhash: Hash::default().to_string(),
                shred_version: SHRED_VERSION as u32,
                wallclock: now,
            }),
        );
        let expected_active_percent =
            (initial_num_active_validators + 2) as f64 / TOTAL_VALIDATOR_COUNT as f64 * 100.0;
        assert_eq!(
            test_state.slots_aggregate.min_active_percent(),
            expected_active_percent
        );
        let mut actual_slots =
            Vec::from_iter(test_state.slots_aggregate.slots_to_repair_iter().cloned());
        actual_slots.sort();
        assert_eq!(actual_slots, test_state.last_voted_fork_slots);

        let replace_message_validator = test_state.validator_voting_keypairs[2]
            .node_keypair
            .pubkey();
        // Allow specific validator to replace message.
        let now = timestamp();
        let replace_message_validator_last_fork = RestartLastVotedForkSlots::new(
            replace_message_validator,
            now,
            &[root_slot + 1, root_slot + 4, root_slot + 5],
            Hash::default(),
            SHRED_VERSION,
        )
        .unwrap();
        assert_eq!(
            test_state
                .slots_aggregate
                .aggregate(replace_message_validator_last_fork),
            Some(LastVotedForkSlotsRecord {
                last_voted_fork_slots: vec![root_slot + 1, root_slot + 4, root_slot + 5],
                last_vote_bankhash: Hash::default().to_string(),
                shred_version: SHRED_VERSION as u32,
                wallclock: now,
            }),
        );
        assert_eq!(
            test_state.slots_aggregate.min_active_percent(),
            expected_active_percent
        );
        let mut actual_slots =
            Vec::from_iter(test_state.slots_aggregate.slots_to_repair_iter().cloned());
        actual_slots.sort();
        assert_eq!(actual_slots, vec![root_slot + 1]);

        // test that message from my pubkey is ignored.
        assert_eq!(
            test_state.slots_aggregate.aggregate(
                RestartLastVotedForkSlots::new(
                    test_state.validator_voting_keypairs[MY_INDEX]
                        .node_keypair
                        .pubkey(),
                    timestamp(),
                    &[root_slot + 1, root_slot + 4, root_slot + 5],
                    Hash::default(),
                    SHRED_VERSION,
                )
                .unwrap(),
            ),
            None,
        );

        assert_eq!(
            test_state.slots_aggregate.get_final_result(),
            LastVotedForkSlotsFinalResult {
                slots_stake_map: vec![
                    (root_slot + 1, 500),
                    (root_slot + 2, 400),
                    (root_slot + 3, 400),
                    (root_slot + 4, 100),
                    (root_slot + 5, 100),
                ]
                .into_iter()
                .collect(),
                epoch_info_vec: vec![LastVotedForkSlotsEpochInfo {
                    epoch: 0,
                    total_stake: 1000,
                    total_active_stake: 500,
                },],
            },
        );
    }

    #[test]
    fn test_aggregate_from_record() {
        let mut test_state = test_aggregate_init();
        let root_slot = test_state.root_slot;
        let last_vote_bankhash = Hash::new_unique();
        let time1 = timestamp();
        let record = LastVotedForkSlotsRecord {
            wallclock: time1,
            last_voted_fork_slots: test_state.last_voted_fork_slots.clone(),
            last_vote_bankhash: last_vote_bankhash.to_string(),
            shred_version: SHRED_VERSION as u32,
        };
        assert_eq!(test_state.slots_aggregate.min_active_percent(), 0.0);
        assert_eq!(
            test_state
                .slots_aggregate
                .aggregate_from_record(
                    &test_state.validator_voting_keypairs[0]
                        .node_keypair
                        .pubkey()
                        .to_string(),
                    &record,
                )
                .unwrap(),
            Some(record.clone()),
        );
        // Before some slot reaches 40% stake, the percentage should be 0.
        assert_eq!(test_state.slots_aggregate.min_active_percent(), 0.0);
        for i in 1..4 {
            assert_eq!(test_state.slots_aggregate.min_active_percent(), 0.0);
            let pubkey = test_state.validator_voting_keypairs[i]
                .node_keypair
                .pubkey();
            let now = timestamp();
            let last_voted_fork_slots = RestartLastVotedForkSlots::new(
                pubkey,
                now,
                &test_state.last_voted_fork_slots,
                last_vote_bankhash,
                SHRED_VERSION,
            )
            .unwrap();
            assert_eq!(
                test_state.slots_aggregate.aggregate(last_voted_fork_slots),
                Some(LastVotedForkSlotsRecord {
                    wallclock: now,
                    last_voted_fork_slots: test_state.last_voted_fork_slots.clone(),
                    last_vote_bankhash: last_vote_bankhash.to_string(),
                    shred_version: SHRED_VERSION as u32,
                }),
            );
        }
        assert_eq!(test_state.slots_aggregate.min_active_percent(), 50.0);
        // Now if you get the same result from Gossip again, it should be ignored.
        assert_eq!(
            test_state.slots_aggregate.aggregate(
                RestartLastVotedForkSlots::new(
                    test_state.validator_voting_keypairs[0]
                        .node_keypair
                        .pubkey(),
                    time1,
                    &test_state.last_voted_fork_slots,
                    last_vote_bankhash,
                    SHRED_VERSION,
                )
                .unwrap(),
            ),
            None,
        );

        // But if it's a new record from the same validator, it will be replaced.
        let time2 = timestamp();
        let last_voted_fork_slots2 =
            vec![root_slot + 1, root_slot + 2, root_slot + 3, root_slot + 4];
        let last_vote_bankhash2 = Hash::new_unique();
        assert_eq!(
            test_state.slots_aggregate.aggregate(
                RestartLastVotedForkSlots::new(
                    test_state.validator_voting_keypairs[0]
                        .node_keypair
                        .pubkey(),
                    time2,
                    &last_voted_fork_slots2,
                    last_vote_bankhash2,
                    SHRED_VERSION,
                )
                .unwrap(),
            ),
            Some(LastVotedForkSlotsRecord {
                wallclock: time2,
                last_voted_fork_slots: last_voted_fork_slots2.clone(),
                last_vote_bankhash: last_vote_bankhash2.to_string(),
                shred_version: SHRED_VERSION as u32,
            }),
        );
        // percentage doesn't change since it's a replace.
        assert_eq!(test_state.slots_aggregate.min_active_percent(), 50.0);

        // Record from my pubkey should be ignored.
        assert_eq!(
            test_state
                .slots_aggregate
                .aggregate_from_record(
                    &test_state.validator_voting_keypairs[MY_INDEX]
                        .node_keypair
                        .pubkey()
                        .to_string(),
                    &LastVotedForkSlotsRecord {
                        wallclock: timestamp(),
                        last_voted_fork_slots: vec![root_slot + 10, root_slot + 300],
                        last_vote_bankhash: Hash::new_unique().to_string(),
                        shred_version: SHRED_VERSION as u32,
                    }
                )
                .unwrap(),
            None,
        );
        assert_eq!(
            test_state.slots_aggregate.get_final_result(),
            LastVotedForkSlotsFinalResult {
                slots_stake_map: vec![
                    (root_slot + 1, 500),
                    (root_slot + 2, 500),
                    (root_slot + 3, 500),
                    (root_slot + 4, 100),
                ]
                .into_iter()
                .collect(),
                epoch_info_vec: vec![LastVotedForkSlotsEpochInfo {
                    epoch: 0,
                    total_stake: 1000,
                    total_active_stake: 500,
                },],
            },
        );
    }

    #[test]
    fn test_aggregate_from_record_failures() {
        solana_logger::setup();
        let mut test_state = test_aggregate_init();
        let last_vote_bankhash = Hash::new_unique();
        let mut last_voted_fork_slots_record = LastVotedForkSlotsRecord {
            wallclock: timestamp(),
            last_voted_fork_slots: test_state.last_voted_fork_slots,
            last_vote_bankhash: last_vote_bankhash.to_string(),
            shred_version: SHRED_VERSION as u32,
        };
        // First test that this is a valid record.
        assert_eq!(
            test_state
                .slots_aggregate
                .aggregate_from_record(
                    &test_state.validator_voting_keypairs[0]
                        .node_keypair
                        .pubkey()
                        .to_string(),
                    &last_voted_fork_slots_record,
                )
                .unwrap(),
            Some(last_voted_fork_slots_record.clone()),
        );
        // Then test that it fails if the record is invalid.

        // Invalid pubkey.
        assert!(test_state
            .slots_aggregate
            .aggregate_from_record("invalid_pubkey", &last_voted_fork_slots_record,)
            .is_err());

        // Invalid hash.
        last_voted_fork_slots_record.last_vote_bankhash.clear();
        assert!(test_state
            .slots_aggregate
            .aggregate_from_record(
                &test_state.validator_voting_keypairs[0]
                    .node_keypair
                    .pubkey()
                    .to_string(),
                &last_voted_fork_slots_record,
            )
            .is_err());
        last_voted_fork_slots_record.last_vote_bankhash.pop();

        // Empty last voted fork.
        last_voted_fork_slots_record.last_vote_bankhash = last_vote_bankhash.to_string();
        last_voted_fork_slots_record.last_voted_fork_slots.clear();
        assert!(test_state
            .slots_aggregate
            .aggregate_from_record(
                &test_state.validator_voting_keypairs[0]
                    .node_keypair
                    .pubkey()
                    .to_string(),
                &last_voted_fork_slots_record,
            )
            .is_err());
    }
}
