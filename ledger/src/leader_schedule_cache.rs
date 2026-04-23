use {
    crate::blockstore::Blockstore,
    itertools::Itertools,
    log::*,
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_leader_schedule::{FixedSchedule, LeaderSchedule, SlotLeader},
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, leader_schedule_utils},
    std::{
        collections::{HashMap, VecDeque, hash_map::Entry},
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
    },
};

type CachedSchedules = (HashMap<Epoch, Arc<LeaderSchedule>>, VecDeque<u64>);
const MAX_SCHEDULES: usize = 10;

struct CacheCapacity(usize);
impl Default for CacheCapacity {
    fn default() -> Self {
        CacheCapacity(MAX_SCHEDULES)
    }
}

#[derive(Default)]
pub struct LeaderScheduleCache {
    // Map from an epoch to a leader schedule for that epoch
    pub cached_schedules: RwLock<CachedSchedules>,
    epoch_schedule: EpochSchedule,
    max_epoch: AtomicU64,
    max_schedules: CacheCapacity,
    fixed_schedule: Option<Arc<FixedSchedule>>,
}

impl LeaderScheduleCache {
    pub fn new_from_bank(bank: &Bank) -> Self {
        Self::new(bank.epoch_schedule().clone(), bank)
    }

    pub fn new(epoch_schedule: EpochSchedule, root_bank: &Bank) -> Self {
        let max_epoch = root_bank.get_leader_schedule_epoch(root_bank.slot());
        let cache = Self {
            cached_schedules: RwLock::new((HashMap::new(), VecDeque::new())),
            epoch_schedule,
            max_epoch: AtomicU64::new(max_epoch),
            max_schedules: CacheCapacity::default(),
            fixed_schedule: None,
        };

        // Calculate the schedule for all epochs in epoch stakes
        let min_epoch = root_bank
            .epoch_stakes_map()
            .keys()
            .min()
            .copied()
            .unwrap_or_default();
        for epoch in min_epoch..=max_epoch {
            cache.compute_leader_schedule(epoch, root_bank);
        }
        cache
    }

    pub fn max_schedules(&self) -> usize {
        self.max_schedules.0
    }

    pub fn set_root(&self, root_bank: &Bank) {
        let new_max_epoch = root_bank.get_leader_schedule_epoch(root_bank.slot());
        let old_max_epoch = self.max_epoch.swap(new_max_epoch, Ordering::AcqRel);
        assert!(new_max_epoch >= old_max_epoch);

        // Calculate the epoch as soon as it's rooted
        if new_max_epoch > old_max_epoch {
            self.compute_leader_schedule(new_max_epoch, root_bank);
        }
    }

    pub fn slot_leader_at(&self, slot: Slot, bank: Option<&Bank>) -> Option<SlotLeader> {
        if let Some(bank) = bank {
            self.slot_leader_at_else_compute(slot, bank)
        } else if self.epoch_schedule.slots_per_epoch == 0 {
            None
        } else {
            self.slot_leader_at_no_compute(slot)
        }
    }

    /// Returns the (next slot, last slot) consecutive range of slots after
    /// the given current_slot that the given node will be leader.
    pub fn next_leader_slot(
        &self,
        pubkey: &Pubkey,
        current_slot: Slot,
        bank: &Bank,
        blockstore: Option<&Blockstore>,
        max_slot_range: u64,
    ) -> Option<(Slot, Slot)> {
        let (epoch, start_index) = bank.get_epoch_and_slot_index(current_slot + 1);
        let max_epoch = self.max_epoch.load(Ordering::Acquire);
        if epoch > max_epoch {
            debug!(
                "Requested next leader in slot: {} of unconfirmed epoch: {}",
                current_slot + 1,
                epoch
            );
            return None;
        }
        // Collect leader schedules first so they stay alive for the iterator chain
        let schedules: Vec<_> = (epoch..=max_epoch)
            .map(|epoch| self.get_leader_schedule_else_compute(epoch, bank))
            .while_some()
            .zip(epoch..)
            .collect();

        // Slots after current_slot where pubkey is the leader.
        let mut schedule = schedules
            .iter()
            .flat_map(|(leader_schedule, k)| {
                let offset = if *k == epoch { start_index as usize } else { 0 };
                let num_slots = bank.get_slots_in_epoch(*k) as usize;
                let first_slot = bank.get_first_slot_in_epoch(*k);
                leader_schedule
                    .get_leader_upcoming_slots(pubkey, offset)
                    .take_while(move |i| *i < num_slots)
                    .map(move |i| i as Slot + first_slot)
            })
            .skip_while(|slot| {
                // Skip slots we already have shreds for
                blockstore
                    .map(|bs| bs.has_existing_shreds_for_slot(*slot))
                    .unwrap_or(false)
            });
        let first_slot = schedule.next()?;
        let max_slot = first_slot.saturating_add(max_slot_range);
        let last_slot = schedule
            .take_while(|slot| *slot < max_slot)
            .zip(first_slot + 1..)
            .take_while(|(a, b)| a == b)
            .map(|(s, _)| s)
            .last()
            .unwrap_or(first_slot);
        Some((first_slot, last_slot))
    }

    pub fn set_fixed_leader_schedule(&mut self, fixed_schedule: Option<FixedSchedule>) {
        self.fixed_schedule = fixed_schedule.map(Arc::new);
    }

    fn slot_leader_at_no_compute(&self, slot: Slot) -> Option<SlotLeader> {
        let (epoch, slot_index) = self.epoch_schedule.get_epoch_and_slot_index(slot);
        if let Some(ref fixed_schedule) = self.fixed_schedule {
            return Some(fixed_schedule.leader_schedule[slot_index]);
        }
        self.cached_schedules
            .read()
            .unwrap()
            .0
            .get(&epoch)
            .map(|schedule| schedule[slot_index])
    }

    fn slot_leader_at_else_compute(&self, slot: Slot, bank: &Bank) -> Option<SlotLeader> {
        let (epoch, slot_index) = bank.get_epoch_and_slot_index(slot);
        // Forbid asking for slots in an unconfirmed epoch
        if epoch > self.max_epoch.load(Ordering::Acquire) {
            debug!("Requested leader in slot: {slot} of unconfirmed epoch: {epoch}");
            return None;
        }
        if let Some(ref fixed_schedule) = self.fixed_schedule {
            Some(fixed_schedule.leader_schedule[slot_index])
        } else if let Some(leader_schedule) = self.get_epoch_leader_schedule(epoch) {
            Some(leader_schedule[slot_index])
        } else {
            self.compute_leader_schedule(epoch, bank)
                .map(|leader_schedule| leader_schedule[slot_index])
        }
    }

    pub fn get_epoch_leader_schedule(&self, epoch: Epoch) -> Option<Arc<LeaderSchedule>> {
        self.cached_schedules.read().unwrap().0.get(&epoch).cloned()
    }

    fn get_leader_schedule_else_compute(
        &self,
        epoch: Epoch,
        bank: &Bank,
    ) -> Option<Arc<LeaderSchedule>> {
        if let Some(ref fixed_schedule) = self.fixed_schedule {
            return Some(fixed_schedule.leader_schedule.clone());
        }
        let epoch_schedule = self.get_epoch_leader_schedule(epoch);
        if epoch_schedule.is_some() {
            epoch_schedule
        } else {
            self.compute_leader_schedule(epoch, bank)
        }
    }

    fn compute_leader_schedule(&self, epoch: Epoch, bank: &Bank) -> Option<Arc<LeaderSchedule>> {
        let leader_schedule = leader_schedule_utils::leader_schedule(epoch, bank);
        leader_schedule.map(|leader_schedule| {
            let leader_schedule = Arc::new(leader_schedule);
            let (ref mut cached_schedules, ref mut order) = *self.cached_schedules.write().unwrap();
            // Check to see if schedule exists in case somebody already inserted in the time we were
            // waiting for the lock
            let entry = cached_schedules.entry(epoch);
            if let Entry::Vacant(v) = entry {
                v.insert(leader_schedule.clone());
                order.push_back(epoch);
                Self::retain_latest(cached_schedules, order, self.max_schedules());
            }
            leader_schedule
        })
    }

    fn retain_latest(
        schedules: &mut HashMap<Epoch, Arc<LeaderSchedule>>,
        order: &mut VecDeque<u64>,
        max_schedules: usize,
    ) {
        while schedules.len() > max_schedules {
            let first = order.pop_front().unwrap();
            schedules.remove(&first);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            blockstore::make_slot_entries,
            genesis_utils::{
                GenesisConfigInfo, bootstrap_validator_stake_lamports, create_genesis_config,
                create_genesis_config_with_leader,
            },
            staking_utils::tests::setup_vote_and_stake_accounts,
        },
        crossbeam_channel::unbounded,
        solana_epoch_schedule::{
            DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET, EpochSchedule, MINIMUM_SLOTS_PER_EPOCH,
        },
        solana_keypair::Keypair,
        solana_leader_schedule::{LeaderSchedule, SlotLeader},
        solana_runtime::{
            genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
            leader_schedule_utils, stake_utils,
        },
        solana_signer::Signer,
        std::{sync::Arc, thread::Builder},
    };

    #[test]
    fn test_new_cache() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
        let bank = Bank::new_for_tests(&genesis_config);
        let cache = LeaderScheduleCache::new_from_bank(&bank);
        assert_eq!(bank.slot(), 0);
        assert_eq!(cache.max_schedules(), MAX_SCHEDULES);

        // Epoch schedule for all epochs in the range:
        // [0, leader_schedule_epoch(bank.slot())] should
        // be calculated by constructor
        let epoch_schedule = bank.epoch_schedule();
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        for epoch in 0..=leader_schedule_epoch {
            let first_slot_in_leader_schedule_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);
            let last_slot_in_leader_schedule_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);
            assert!(
                cache
                    .slot_leader_at(first_slot_in_leader_schedule_epoch, None)
                    .is_some()
            );
            assert!(
                cache
                    .slot_leader_at(last_slot_in_leader_schedule_epoch, None)
                    .is_some()
            );
            if epoch == leader_schedule_epoch {
                assert!(
                    cache
                        .slot_leader_at(last_slot_in_leader_schedule_epoch + 1, None)
                        .is_none()
                );
            }
        }

        let (cached_schedules, order) = &*cache.cached_schedules.read().unwrap();

        // Should be a schedule for every epoch just checked
        assert_eq!(cached_schedules.len() as u64, leader_schedule_epoch + 1);

        // Order should contain every epoch in order of lowest to highest
        assert_eq!(order, &VecDeque::from_iter(0..=leader_schedule_epoch));
    }

    #[test]
    fn test_retain_latest() {
        let mut cached_schedules: HashMap<Epoch, Arc<LeaderSchedule>> = HashMap::new();
        let mut order = VecDeque::new();
        for i in 0..=MAX_SCHEDULES {
            cached_schedules.insert(i as u64, Arc::new(LeaderSchedule::default()));
            order.push_back(i as u64);
        }
        LeaderScheduleCache::retain_latest(&mut cached_schedules, &mut order, MAX_SCHEDULES);
        assert_eq!(cached_schedules.len(), MAX_SCHEDULES);
        let mut keys: Vec<_> = cached_schedules.keys().cloned().collect();
        keys.sort_unstable();
        let expected: Vec<_> = (1..=MAX_SCHEDULES as u64).collect();
        let expected_order: VecDeque<_> = (1..=MAX_SCHEDULES as u64).collect();
        assert_eq!(expected, keys);
        assert_eq!(expected_order, order);
    }

    #[test]
    fn test_thread_race_leader_schedule_cache() {
        let num_runs = 10;
        for _ in 0..num_runs {
            run_thread_race()
        }
    }

    fn run_thread_race() {
        let slots_per_epoch = MINIMUM_SLOTS_PER_EPOCH;
        let epoch_schedule = EpochSchedule::custom(slots_per_epoch, slots_per_epoch / 2, true);
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        genesis_config.epoch_schedule = epoch_schedule.clone();
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let cache = Arc::new(LeaderScheduleCache::new(epoch_schedule, &bank));

        let num_threads = 10;
        let (threads, senders): (Vec<_>, Vec<_>) = (0..num_threads)
            .map(|_| {
                let cache = cache.clone();
                let bank = bank.clone();
                let (sender, receiver) = unbounded();
                (
                    Builder::new()
                        .name("test_thread_race_leader_schedule_cache".to_string())
                        .spawn(move || {
                            let _ = receiver.recv();
                            cache.slot_leader_at(bank.slot(), Some(&bank));
                        })
                        .unwrap(),
                    sender,
                )
            })
            .unzip();

        for sender in &senders {
            sender.send(true).unwrap();
        }

        for t in threads.into_iter() {
            t.join().unwrap();
        }

        let (ref cached_schedules, ref order) = *cache.cached_schedules.read().unwrap();
        assert_eq!(cached_schedules.len(), 1);
        assert_eq!(order.len(), 1);
    }

    #[test]
    fn test_next_leader_slot() {
        let pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            mut genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config_with_leader(42, &pubkey, bootstrap_validator_stake_lamports());
        let slot_leader = SlotLeader {
            id: pubkey,
            vote_address: voting_keypair.pubkey(),
        };
        genesis_config.epoch_schedule =
            EpochSchedule::custom(MINIMUM_SLOTS_PER_EPOCH, MINIMUM_SLOTS_PER_EPOCH, false);

        let bank = Bank::new_for_tests(&genesis_config);
        let cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let max_generated_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let last_confirmed_schedule_slot = bank.get_last_slot_in_epoch(max_generated_epoch);

        assert_eq!(
            cache.slot_leader_at(bank.slot(), Some(&bank)).unwrap(),
            slot_leader
        );
        assert_eq!(
            cache.next_leader_slot(&pubkey, 0, &bank, None, u64::MAX),
            Some((1, last_confirmed_schedule_slot))
        );
        assert_eq!(
            cache.next_leader_slot(&pubkey, 1, &bank, None, u64::MAX),
            Some((2, last_confirmed_schedule_slot))
        );
        assert_eq!(
            cache.next_leader_slot(
                &pubkey,
                last_confirmed_schedule_slot, // no schedule generated for the next epoch
                &bank,
                None,
                u64::MAX
            ),
            None
        );

        assert_eq!(
            cache.next_leader_slot(
                &solana_pubkey::new_rand(), // not in leader_schedule
                0,
                &bank,
                None,
                u64::MAX
            ),
            None
        );
    }

    #[test]
    fn test_next_leader_slot_blockstore() {
        let pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            mut genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config_with_leader(42, &pubkey, bootstrap_validator_stake_lamports());
        let slot_leader = SlotLeader {
            id: pubkey,
            vote_address: voting_keypair.pubkey(),
        };
        genesis_config.epoch_schedule =
            EpochSchedule::custom(MINIMUM_SLOTS_PER_EPOCH, MINIMUM_SLOTS_PER_EPOCH, false);

        let bank = Bank::new_for_tests(&genesis_config);
        let cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let max_generated_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let last_confirmed_schedule_slot = bank.get_last_slot_in_epoch(max_generated_epoch);
        let ledger_path = get_tmp_ledger_path_auto_delete!();

        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");

        assert_eq!(
            cache.slot_leader_at(bank.slot(), Some(&bank)).unwrap(),
            slot_leader,
        );
        // Check that the next leader slot after 0 is slot 1
        assert_eq!(
            cache
                .next_leader_slot(&pubkey, 0, &bank, Some(&blockstore), u64::MAX)
                .unwrap()
                .0,
            1
        );

        // Write a shred into slot 2 that chains to slot 1,
        // but slot 1 is empty so should not be skipped
        let (shreds, _) = make_slot_entries(2, 1, 1);
        blockstore.insert_shreds(shreds, None, false).unwrap();
        assert_eq!(
            cache
                .next_leader_slot(&pubkey, 0, &bank, Some(&blockstore), u64::MAX)
                .unwrap()
                .0,
            1
        );

        // Write a shred into slot 1
        let (shreds, _) = make_slot_entries(1, 0, 1);

        // Check that slot 1 and 2 are skipped
        blockstore.insert_shreds(shreds, None, false).unwrap();
        assert_eq!(
            cache
                .next_leader_slot(&pubkey, 0, &bank, Some(&blockstore), u64::MAX)
                .unwrap()
                .0,
            3
        );

        // Integrity checks
        assert_eq!(
            cache.next_leader_slot(
                &pubkey,
                last_confirmed_schedule_slot, // no schedule generated for the next epoch
                &bank,
                Some(&blockstore),
                u64::MAX
            ),
            None
        );

        assert_eq!(
            cache.next_leader_slot(
                &solana_pubkey::new_rand(), // not in leader_schedule
                0,
                &bank,
                Some(&blockstore),
                u64::MAX
            ),
            None
        );
    }

    #[test]
    fn test_next_leader_slot_next_epoch() {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000 * bootstrap_validator_stake_lamports());
        genesis_config.epoch_schedule =
            EpochSchedule::custom(MINIMUM_SLOTS_PER_EPOCH, MINIMUM_SLOTS_PER_EPOCH, false);

        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));

        // Create new vote account
        let validator_identity = Keypair::new();
        let vote_account = Keypair::new();
        setup_vote_and_stake_accounts(
            &bank,
            &mint_keypair,
            &vote_account,
            &validator_identity,
            bootstrap_validator_stake_lamports()
                + stake_utils::get_minimum_delegation(
                    bank.feature_set.snapshot().upgrade_bpf_stake_program_to_v5,
                ),
        );
        let node_pubkey = validator_identity.pubkey();

        // Have to wait until the epoch at after the epoch stakes generated at genesis
        // for the new votes to take effect.
        let mut target_slot = 1;
        let epoch = bank.get_leader_schedule_epoch(0);
        while bank.get_leader_schedule_epoch(target_slot) == epoch {
            target_slot += 1;
        }

        let child_bank = Bank::new_from_parent(bank.clone(), SlotLeader::default(), target_slot);
        let bank = bank_forks
            .write()
            .unwrap()
            .insert(child_bank)
            .clone_without_scheduler();
        let mut expected_slot = 0;
        let epoch = bank.get_leader_schedule_epoch(target_slot);
        for i in 0..epoch {
            expected_slot += bank.get_slots_in_epoch(i);
        }

        let schedule = cache.compute_leader_schedule(epoch, &bank).unwrap();
        let mut index = 0;
        while schedule[index].id != node_pubkey {
            index += 1;
            assert_ne!(index, genesis_config.epoch_schedule.slots_per_epoch);
        }
        expected_slot += index;

        // If the max root isn't set, we'll get None
        assert!(
            cache
                .next_leader_slot(&node_pubkey, 0, &bank, None, u64::MAX)
                .is_none()
        );

        cache.set_root(&bank);
        let res = cache
            .next_leader_slot(&node_pubkey, 0, &bank, None, u64::MAX)
            .unwrap();
        let leader_span = bank.num_consecutive_leader_slots_for_epoch(epoch);

        assert_eq!(res.0, expected_slot);
        assert!(res.1 >= expected_slot + leader_span - 1);

        let res = cache
            .next_leader_slot(&node_pubkey, 0, &bank, None, leader_span - 1)
            .unwrap();

        assert_eq!(res.0, expected_slot);
        assert_eq!(res.1, expected_slot + leader_span - 2);
    }

    #[test]
    fn test_schedule_for_unconfirmed_epoch() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        genesis_config.epoch_schedule = EpochSchedule::custom(
            MINIMUM_SLOTS_PER_EPOCH,
            DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET.min(MINIMUM_SLOTS_PER_EPOCH),
            true,
        );
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let cache = LeaderScheduleCache::new_from_bank(&bank);
        let last_slot_in_epoch_1 = bank.get_last_slot_in_epoch(1);
        let first_slot_in_epoch_2 = bank.get_first_slot_in_epoch(2);

        assert_eq!(cache.max_epoch.load(Ordering::Acquire), 1);

        // Asking for the leader for the last slot in epoch 1 is ok b/c
        // epoch 1 is confirmed
        assert_eq!(bank.get_epoch_and_slot_index(last_slot_in_epoch_1).0, 1);
        assert!(
            cache
                .slot_leader_at(last_slot_in_epoch_1, Some(&bank))
                .is_some()
        );

        // Asking for the lader for the first slot in epoch 2 is not ok
        // b/c epoch 2 is unconfirmed
        assert_eq!(bank.get_epoch_and_slot_index(first_slot_in_epoch_2).0, 2);
        assert!(
            cache
                .slot_leader_at(first_slot_in_epoch_2, Some(&bank))
                .is_none()
        );

        let bank2 = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::new_unique(),
            last_slot_in_epoch_1,
        );
        assert!(bank2.epoch_vote_accounts(2).is_some());

        // Set root for a slot in epoch 1, so that epoch 2 is now confirmed
        cache.set_root(bank2.as_ref());
        assert_eq!(cache.max_epoch.load(Ordering::Acquire), 2);
        assert!(
            cache
                .slot_leader_at(first_slot_in_epoch_2, Some(bank2.as_ref()))
                .is_some()
        );
        let last_slot_in_epoch_2 = bank2.get_last_slot_in_epoch(2);
        let first_slot_in_epoch_3 = bank2.get_first_slot_in_epoch(3);
        assert_eq!(bank2.get_epoch_and_slot_index(last_slot_in_epoch_2).0, 2);
        assert!(
            cache
                .slot_leader_at(last_slot_in_epoch_2, Some(bank2.as_ref()))
                .is_some()
        );
        assert_eq!(bank2.get_epoch_and_slot_index(first_slot_in_epoch_3).0, 3);
        assert!(
            cache
                .slot_leader_at(first_slot_in_epoch_3, Some(bank2.as_ref()))
                .is_none()
        );
    }

    #[test]
    fn test_slot_leader_at_uses_bank_epoch_math_for_slot_timing_transitions() {
        let validator_keypairs = vec![
            ValidatorVoteKeypairs::new_rand(),
            ValidatorVoteKeypairs::new_rand(),
            ValidatorVoteKeypairs::new_rand(),
        ];
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config_with_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![
                bootstrap_validator_stake_lamports(),
                bootstrap_validator_stake_lamports() * 2,
                bootstrap_validator_stake_lamports() * 3,
            ],
        );
        genesis_config.epoch_schedule =
            EpochSchedule::custom(MINIMUM_SLOTS_PER_EPOCH, MINIMUM_SLOTS_PER_EPOCH, false);

        let bank = Bank::new_for_tests(&genesis_config);
        let cache = LeaderScheduleCache::new_from_bank(&bank);
        let base_slots_per_epoch = genesis_config.epoch_schedule.slots_per_epoch;
        let transitioned_slots_per_epoch = bank.get_slots_in_epoch(0);

        assert!(transitioned_slots_per_epoch > base_slots_per_epoch);

        let slot_in_extended_epoch_zero = base_slots_per_epoch;
        assert_eq!(bank.get_epoch(slot_in_extended_epoch_zero), 0);

        let expected_leader =
            leader_schedule_utils::slot_leader_at(slot_in_extended_epoch_zero, &bank).unwrap();
        let cached_leader = cache
            .slot_leader_at(slot_in_extended_epoch_zero, Some(&bank))
            .unwrap();

        assert_eq!(cached_leader.id, expected_leader);
    }
}
