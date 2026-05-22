use {
    super::{
        Bank, EpochRewardStatus, PartitionedStakeReward, StakeRewards,
        StartBlockHeightAndPartitionedRewards, epoch_rewards_hasher,
    },
    crate::{
        bank::{
            metrics::{RewardsStoreMetrics, report_partitioned_reward_metrics},
            partitioned_epoch_rewards::EpochRewardPhase,
        },
        stake_account::StakeAccount,
    },
    log::error,
    serde::{Deserialize, Serialize},
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMut},
    solana_accounts_db::stake_rewards::{StakeReward, StakeRewardInfo},
    solana_clock::Epoch,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_reward_info::RewardType,
    solana_stake_interface::{
        stake_history::StakeHistory,
        state::{Delegation, StakeStateV2},
    },
    std::sync::{Arc, atomic::Ordering::Relaxed},
    thiserror::Error,
};

#[derive(Serialize, Deserialize, Debug, Error, PartialEq, Eq, Clone)]
enum DistributionError {
    #[error("stake account not found")]
    AccountNotFound,

    #[error("rewards arithmetic overflowed")]
    ArithmeticOverflow,

    #[error("stake account set_state failed")]
    UnableToSetState,
}

struct DistributionResults {
    lamports_distributed: u64,
    lamports_burned: u64,
    updated_stake_rewards: StakeRewards,
}

impl Bank {
    /// Process reward distribution for the block if it is inside reward interval.
    pub(in crate::bank) fn distribute_partitioned_epoch_rewards(&mut self) {
        let EpochRewardStatus::Active(status) = &self.epoch_reward_status else {
            return;
        };

        let distribution_starting_block_height = match &status {
            EpochRewardPhase::Calculation(status) => status.distribution_starting_block_height,
            EpochRewardPhase::Distribution(status) => status.distribution_starting_block_height,
        };

        let height = self.block_height();
        if height < distribution_starting_block_height {
            return;
        }

        if let EpochRewardPhase::Calculation(status) = &status {
            // epoch rewards have not been partitioned yet, so partition them now
            // This should happen only once immediately on the first rewards distribution block, after reward calculation block.
            let epoch_rewards_sysvar = self.get_epoch_rewards_sysvar();
            let (partition_indices, partition_us) = measure_us!({
                epoch_rewards_hasher::hash_rewards_into_partitions(
                    &status.all_stake_rewards,
                    &epoch_rewards_sysvar.parent_blockhash,
                    epoch_rewards_sysvar.num_partitions as usize,
                )
            });

            // update epoch reward status to distribution phase
            self.set_epoch_reward_status_distribution(
                distribution_starting_block_height,
                Arc::clone(&status.all_stake_rewards),
                partition_indices,
            );

            datapoint_info!(
                "epoch-rewards-status-update",
                ("slot", self.slot(), i64),
                ("block_height", height, i64),
                ("partition_us", partition_us, i64),
                (
                    "distribution_starting_block_height",
                    distribution_starting_block_height,
                    i64
                ),
            );
        }

        let EpochRewardStatus::Active(EpochRewardPhase::Distribution(partition_rewards)) =
            &self.epoch_reward_status
        else {
            // We should never get here.
            unreachable!(
                "epoch rewards status is not in distribution phase, but we are trying to \
                 distribute rewards"
            );
        };

        let distribution_end_exclusive =
            distribution_starting_block_height + partition_rewards.partition_indices.len() as u64;

        assert!(
            self.epoch_schedule.get_slots_in_epoch(self.epoch)
                > partition_rewards.partition_indices.len() as u64
        );

        if height >= distribution_starting_block_height && height < distribution_end_exclusive {
            let partition_index = height - distribution_starting_block_height;

            self.distribute_epoch_rewards_in_partition(partition_rewards, partition_index);
        }

        if height.saturating_add(1) >= distribution_end_exclusive {
            datapoint_info!(
                "epoch-rewards-status-update",
                ("slot", self.slot(), i64),
                ("block_height", height, i64),
                ("active", 0, i64),
                (
                    "distribution_starting_block_height",
                    distribution_starting_block_height,
                    i64
                ),
            );

            assert!(matches!(
                self.epoch_reward_status,
                EpochRewardStatus::Active(EpochRewardPhase::Distribution(_))
            ));
            self.epoch_reward_status = EpochRewardStatus::Inactive;
            self.set_epoch_rewards_sysvar_to_inactive();
        }
    }

    /// Process reward credits for a partition of rewards
    /// Store the rewards to AccountsDB, update reward history record and total capitalization.
    fn distribute_epoch_rewards_in_partition(
        &self,
        partition_rewards: &StartBlockHeightAndPartitionedRewards,
        partition_index: u64,
    ) {
        let pre_capitalization = self.capitalization();
        let (
            DistributionResults {
                lamports_distributed,
                lamports_burned,
                updated_stake_rewards,
            },
            store_stake_accounts_us,
        ) = measure_us!(self.store_stake_accounts_in_partition(partition_rewards, partition_index));

        // increase total capitalization by the distributed rewards
        self.capitalization.fetch_add(lamports_distributed, Relaxed);

        // decrease distributed capital from epoch rewards sysvar
        self.update_epoch_rewards_sysvar(lamports_distributed + lamports_burned);

        // update reward history for this partitioned distribution
        self.update_reward_history_in_partition(&updated_stake_rewards);

        let metrics = RewardsStoreMetrics {
            pre_capitalization,
            post_capitalization: self.capitalization(),
            total_stake_accounts_count: partition_rewards.all_stake_rewards.num_rewards(),
            total_num_partitions: partition_rewards.partition_indices.len(),
            partition_index,
            store_stake_accounts_us,
            store_stake_accounts_count: updated_stake_rewards.len(),
            distributed_rewards: lamports_distributed,
            burned_rewards: lamports_burned,
        };

        report_partitioned_reward_metrics(self, metrics);
    }

    /// insert non-zero stake rewards to self.rewards
    /// Return the number of rewards inserted
    fn update_reward_history_in_partition(&self, stake_rewards: &[StakeReward]) -> usize {
        let mut rewards = self.rewards.write().unwrap();
        rewards.reserve(stake_rewards.len());
        let initial_len = rewards.len();
        stake_rewards
            .iter()
            .filter(|x| x.get_stake_reward() > 0)
            .for_each(|x| rewards.push((x.stake_pubkey, x.stake_reward_info.into())));
        rewards.len().saturating_sub(initial_len)
    }

    fn build_updated_stake_reward(
        distribution_epoch: u64,
        stake_history: &StakeHistory,
        new_warmup_cooldown_rate_epoch: Option<Epoch>,
        stakes_cache_accounts: &imbl::HashMap<Pubkey, StakeAccount<Delegation>>,
        partitioned_stake_reward: &PartitionedStakeReward,
        rent: &Rent,
        adjust_delegations_for_rent: bool,
    ) -> Result<StakeReward, DistributionError> {
        let stake_account = stakes_cache_accounts
            .get(&partitioned_stake_reward.stake_pubkey)
            .ok_or(DistributionError::AccountNotFound)?
            .clone();

        let (mut account, stake_state): (AccountSharedData, StakeStateV2) = stake_account.into();
        let StakeStateV2::Stake(meta, stake, flags) = stake_state else {
            // StakesCache only stores accounts where StakeStateV2::delegation().is_some()
            unreachable!(
                "StakesCache entry {:?} failed StakeStateV2 deserialization",
                partitioned_stake_reward.stake_pubkey
            )
        };
        account
            .checked_add_lamports(partitioned_stake_reward.stake_reward)
            .map_err(|_| DistributionError::ArithmeticOverflow)?;
        if adjust_delegations_for_rent {
            let minimum_balance = rent.minimum_balance(account.data().len());
            assert!(
                partitioned_stake_reward.stake.delegation.stake
                    <= account.lamports().saturating_sub(minimum_balance),
                "stake reward delegation must be consistent with the updated stake account \
                 lamport balance"
            );
        } else {
            let expected_delegation = stake
                .delegation
                .stake
                .saturating_add(partitioned_stake_reward.stake_reward);
            assert_eq!(
                expected_delegation, partitioned_stake_reward.stake.delegation.stake,
                "stake reward delegation must be consistent with the updated stake account \
                 lamport balance"
            );
        }
        account
            .set_state(&StakeStateV2::Stake(
                meta,
                partitioned_stake_reward.stake,
                flags,
            ))
            .map_err(|_| DistributionError::UnableToSetState)?;

        #[allow(deprecated)]
        let stake_at_distribution_epoch = partitioned_stake_reward.stake.delegation.stake(
            distribution_epoch,
            stake_history,
            new_warmup_cooldown_rate_epoch,
        );
        let reward_type = if stake_at_distribution_epoch == 0 {
            RewardType::DeactivatedStake
        } else {
            RewardType::Staking
        };
        Ok(StakeReward {
            stake_pubkey: partitioned_stake_reward.stake_pubkey,
            stake_reward_info: StakeRewardInfo {
                reward_type,
                lamports: i64::try_from(partitioned_stake_reward.stake_reward).unwrap(),
                post_balance: account.lamports(),
                commission_bps: partitioned_stake_reward.commission_bps,
            },
            stake_account: account,
        })
    }

    /// Store stake rewards in partition
    /// Returns DistributionResults containing the sum of all the rewards
    /// stored, the sum of all rewards burned, and the updated StakeRewards.
    /// Because stake accounts are checked in calculation, and further state
    /// mutation prevents by stake-program restrictions, there should never be
    /// rewards burned.
    ///
    /// Note: even if staker's reward is 0, the stake account still needs to be
    /// stored because credits observed has changed
    fn store_stake_accounts_in_partition(
        &self,
        partition_rewards: &StartBlockHeightAndPartitionedRewards,
        partition_index: u64,
    ) -> DistributionResults {
        let feature_snapshot = self.feature_set.snapshot();
        // Name intentionally doesn't match -- "adjust delegations for rent" is
        // part of relaxing post-exec min balance checks.
        let adjust_delegations_for_rent = feature_snapshot.relax_post_exec_min_balance_check;

        let mut lamports_distributed = 0;
        let mut lamports_burned = 0;
        let indices = partition_rewards
            .partition_indices
            .get(partition_index as usize)
            .unwrap_or_else(|| {
                panic!(
                    "partition index out of bound: {partition_index} >= {}",
                    partition_rewards.partition_indices.len()
                )
            });
        let mut updated_stake_rewards = Vec::with_capacity(indices.len());
        let stakes_cache = self.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        let stake_history = stakes_cache.history();
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        let rent = &self.rent_collector.rent;
        for index in indices {
            let partitioned_stake_reward = partition_rewards
                .all_stake_rewards
                .get(*index)
                .unwrap_or_else(|| {
                    panic!(
                        "partition reward out of bound: {index} >= {}",
                        partition_rewards.all_stake_rewards.total_len()
                    )
                })
                .as_ref()
                .unwrap_or_else(|| {
                    panic!("partition reward {index} is empty");
                });
            let stake_pubkey = partitioned_stake_reward.stake_pubkey;
            let reward_amount = partitioned_stake_reward.stake_reward;
            match Self::build_updated_stake_reward(
                self.epoch,
                stake_history,
                new_warmup_cooldown_rate_epoch,
                stakes_cache_accounts,
                partitioned_stake_reward,
                rent,
                adjust_delegations_for_rent,
            ) {
                Ok(stake_reward) => {
                    lamports_distributed += reward_amount;
                    updated_stake_rewards.push(stake_reward);
                }
                Err(err) => {
                    error!(
                        "bank::distribution::store_stake_accounts_in_partition() failed for \
                         {stake_pubkey}, {reward_amount} lamports burned: {err:?}"
                    );
                    lamports_burned += reward_amount;
                }
            }
        }
        drop(stakes_cache);
        self.store_accounts((self.slot(), &updated_stake_rewards[..]));
        DistributionResults {
            lamports_distributed,
            lamports_burned,
            updated_stake_rewards,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::{
                partitioned_epoch_rewards::{
                    PartitionedStakeRewards, REWARD_CALCULATION_NUM_BLOCKS,
                    epoch_rewards_hasher::hash_rewards_into_partitions, tests::convert_rewards,
                },
                tests::create_genesis_config,
            },
            inflation_rewards::points::PointValue,
            reward_info::RewardInfo,
            stake_utils,
        },
        rand::Rng,
        solana_account::from_account,
        solana_accounts_db::stake_rewards::StakeReward,
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_reward_info::RewardType,
        solana_stake_interface::{
            stake_flags::StakeFlags,
            stake_history::StakeHistoryEntry,
            state::{Meta, Stake},
        },
        solana_sysvar as sysvar,
        solana_vote_interface::state::BLS_PUBLIC_KEY_COMPRESSED_SIZE,
        solana_vote_program::vote_state,
        std::sync::Arc,
        test_case::test_case,
    };

    #[test]
    fn test_distribute_partitioned_epoch_rewards() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let mut bank = Bank::new_for_tests(&genesis_config);

        let expected_num = 100;

        let stake_rewards = (0..expected_num)
            .map(|_| {
                Some(PartitionedStakeReward::new_random(
                    &bank.rent_collector.rent,
                ))
            })
            .collect::<PartitionedStakeRewards>();

        let partition_indices =
            hash_rewards_into_partitions(&stake_rewards, &Hash::new_from_array([1; 32]), 2);

        bank.set_epoch_reward_status_distribution(
            bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            Arc::new(stake_rewards),
            partition_indices,
        );

        bank.distribute_partitioned_epoch_rewards();
    }

    #[test]
    #[should_panic(expected = "self.epoch_schedule.get_slots_in_epoch")]
    fn test_distribute_partitioned_epoch_rewards_too_many_partitions() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let mut bank = Bank::new_for_tests(&genesis_config);

        let expected_num = 1;

        let stake_rewards = (0..expected_num)
            .map(|_| {
                Some(PartitionedStakeReward::new_random(
                    &bank.rent_collector.rent,
                ))
            })
            .collect::<PartitionedStakeRewards>();

        let partition_indices = hash_rewards_into_partitions(
            &stake_rewards,
            &Hash::new_from_array([1; 32]),
            bank.epoch_schedule().slots_per_epoch as usize + 1,
        );

        bank.set_epoch_reward_status_distribution(
            bank.block_height(),
            Arc::new(stake_rewards),
            partition_indices,
        );

        bank.distribute_partitioned_epoch_rewards();
    }

    #[test]
    fn test_distribute_partitioned_epoch_rewards_empty() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let mut bank = Bank::new_for_tests(&genesis_config);

        bank.set_epoch_reward_status_distribution(
            bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            Arc::new(PartitionedStakeRewards::default()),
            vec![],
        );

        bank.distribute_partitioned_epoch_rewards();
    }

    fn populate_starting_stake_accounts_from_stake_rewards(bank: &Bank, rewards: &[StakeReward]) {
        let rent = &bank.rent_collector.rent;
        let validator_pubkey = Pubkey::new_unique();
        let validator_vote_pubkey = Pubkey::new_unique();

        let validator_vote_account = vote_state::create_v4_account_with_authorized(
            &validator_pubkey,
            &validator_vote_pubkey,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &validator_vote_pubkey,
            1000,
            &validator_vote_pubkey,
            0,
            &validator_pubkey,
            20,
        );

        for stake_reward in rewards.iter() {
            // store account in Bank, since distribution now checks for account existence
            let lamports = stake_reward.stake_account.lamports()
                - stake_reward.stake_reward_info.lamports as u64;
            let validator_stake_account = stake_utils::create_stake_account(
                &stake_reward.stake_pubkey,
                &validator_vote_pubkey,
                &validator_vote_account,
                rent,
                lamports,
            );
            bank.store_account(&stake_reward.stake_pubkey, &validator_stake_account);
        }
    }

    /// Test distribute partitioned epoch rewards
    #[test]
    fn test_distribute_partitioned_epoch_rewards_bank_capital_and_sysvar_balance() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(432000, 432000, false);
        let bank = Bank::new_for_tests(&genesis_config);

        // Set up epoch_rewards sysvar with rewards with 1e9 lamports to distribute.
        let total_rewards = 1_000_000_000;
        let num_partitions = 2; // num_partitions is arbitrary and unimportant for this test
        let total_points = (total_rewards * 42) as u128; // total_points is arbitrary for the purposes of this test
        bank.create_epoch_rewards_sysvar(
            0,
            42,
            num_partitions,
            &PointValue {
                rewards: total_rewards,
                points: total_points,
            },
        );
        let pre_epoch_rewards_account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();
        let expected_balance =
            bank.get_minimum_balance_for_rent_exemption(pre_epoch_rewards_account.data().len());
        // Expected balance is the sysvar rent-exempt balance
        assert_eq!(pre_epoch_rewards_account.lamports(), expected_balance);

        // Set up a partition of rewards to distribute
        let expected_num = 100;
        let stake_rewards = (0..expected_num)
            .map(|_| StakeReward::new_random(&bank.rent_collector.rent))
            .collect::<Vec<_>>();
        let rewards_to_distribute = stake_rewards
            .iter()
            .map(|stake_reward| stake_reward.stake_reward_info.lamports)
            .sum::<i64>() as u64;
        populate_starting_stake_accounts_from_stake_rewards(&bank, &stake_rewards);
        let all_rewards = convert_rewards(stake_rewards);

        let partitioned_rewards = StartBlockHeightAndPartitionedRewards {
            distribution_starting_block_height: bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            all_stake_rewards: Arc::new(all_rewards),
            partition_indices: vec![(0..expected_num as usize).collect::<Vec<_>>()],
        };

        // Distribute rewards
        let pre_cap = bank.capitalization();
        bank.distribute_epoch_rewards_in_partition(&partitioned_rewards, 0);
        let post_cap = bank.capitalization();
        let post_epoch_rewards_account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();

        // Assert that epoch rewards sysvar lamports balance does not change
        assert_eq!(post_epoch_rewards_account.lamports(), expected_balance);

        let epoch_rewards: sysvar::epoch_rewards::EpochRewards =
            from_account(&post_epoch_rewards_account).unwrap();
        assert_eq!(epoch_rewards.total_rewards, total_rewards);
        assert_eq!(epoch_rewards.distributed_rewards, rewards_to_distribute,);

        // Assert that the bank total capital changed by the amount of rewards
        // distributed
        assert_eq!(pre_cap + rewards_to_distribute, post_cap);
    }

    /// Test partitioned credits and reward history updates of epoch rewards do cover all the rewards
    /// slice.
    #[test]
    fn test_epoch_credit_rewards_and_history_update() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(432000, 432000, false);
        let bank = Bank::new_for_tests(&genesis_config);

        // setup the expected number of stake rewards
        let expected_num = 12345;

        let mut stake_rewards = (0..expected_num)
            .map(|_| StakeReward::new_random(&bank.rent_collector.rent))
            .collect::<Vec<_>>();
        populate_starting_stake_accounts_from_stake_rewards(&bank, &stake_rewards);

        let expected_rewards = stake_rewards
            .iter()
            .map(|stake_reward| stake_reward.stake_reward_info.lamports)
            .sum::<i64>() as u64;

        // Push extra StakeReward to simulate non-existent account
        stake_rewards.push(StakeReward::new_random(&bank.rent_collector.rent));

        let stake_rewards = convert_rewards(stake_rewards);

        let partition_indices =
            hash_rewards_into_partitions(&stake_rewards, &Hash::new_from_array([1; 32]), 100);
        let num_partitions = partition_indices.len();
        let partitioned_rewards = StartBlockHeightAndPartitionedRewards {
            distribution_starting_block_height: bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            all_stake_rewards: Arc::new(stake_rewards),
            partition_indices,
        };

        // Test partitioned stores
        let mut total_rewards = 0;
        let mut total_num_updates = 0;

        let pre_update_history_len = bank.rewards.read().unwrap().len();

        for i in 0..num_partitions {
            let DistributionResults {
                lamports_distributed,
                updated_stake_rewards,
                ..
            } = bank.store_stake_accounts_in_partition(&partitioned_rewards, i as u64);
            let num_history_updates =
                bank.update_reward_history_in_partition(&updated_stake_rewards);
            assert_eq!(updated_stake_rewards.len(), num_history_updates);
            total_rewards += lamports_distributed;
            total_num_updates += num_history_updates;
        }

        let post_update_history_len = bank.rewards.read().unwrap().len();

        // assert that all rewards are credited
        assert_eq!(total_rewards, expected_rewards);
        assert_eq!(total_num_updates, expected_num);
        assert_eq!(
            total_num_updates,
            post_update_history_len - pre_update_history_len
        );
    }

    #[test]
    fn test_update_reward_history_in_partition() {
        for zero_reward in [false, true] {
            let (genesis_config, _mint_keypair) =
                create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
            let bank = Bank::new_for_tests(&genesis_config);

            let mut expected_num = 100;

            let mut stake_rewards = (0..expected_num)
                .map(|_| StakeReward::new_random(&bank.rent_collector.rent))
                .collect::<Vec<_>>();

            let mut rng = rand::rng();
            let i_zero = rng.random_range(0..expected_num);
            if zero_reward {
                // pick one entry to have zero rewards so it gets ignored
                stake_rewards[i_zero].stake_reward_info.lamports = 0;
            }

            let num_in_history = bank.update_reward_history_in_partition(&stake_rewards);

            if zero_reward {
                stake_rewards.remove(i_zero);
                // -1 because one of them had zero rewards and was ignored
                expected_num -= 1;
            }

            bank.rewards
                .read()
                .unwrap()
                .iter()
                .zip(stake_rewards.iter())
                .for_each(|((k, reward_info), expected_stake_reward)| {
                    assert_eq!(
                        (
                            &expected_stake_reward.stake_pubkey,
                            &RewardInfo::from(expected_stake_reward.stake_reward_info),
                        ),
                        (k, reward_info)
                    );
                });

            assert_eq!(num_in_history, expected_num);
        }
    }

    #[test_case(true; "adjust_delegations_for_rent")]
    #[test_case(false; "no_adjust_delegations_for_rent")]
    fn test_build_updated_stake_reward(adjust_delegations_for_rent: bool) {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        // add an entry so we can get full deactivation one epoch later
        let mut stake_history = StakeHistory::default();
        stake_history.add(
            0,
            StakeHistoryEntry {
                effective: 1_000_000 * LAMPORTS_PER_SOL,
                activating: LAMPORTS_PER_SOL,
                deactivating: LAMPORTS_PER_SOL,
            },
        );

        let distribution_epoch = bank.epoch + 1;
        let new_warmup_cooldown_rate_epoch = bank.new_warmup_cooldown_rate_epoch();
        let mut rent = bank.rent_collector.rent.clone();
        let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());

        // Adjust rent down, no impact at all
        if adjust_delegations_for_rent {
            rent.lamports_per_byte /= 2;
        }

        let voter_pubkey = Pubkey::new_unique();
        let new_stake = Stake {
            delegation: Delegation {
                voter_pubkey,
                stake: 55_555,
                ..Delegation::default()
            },
            credits_observed: 42,
        };
        let stake_reward = 100;
        let commission_bps = 4_200;

        let nonexistent_account = Pubkey::new_unique();
        let partitioned_stake_reward = PartitionedStakeReward {
            stake_pubkey: nonexistent_account,
            stake: new_stake,
            stake_reward,
            commission_bps: Some(commission_bps),
        };
        let stakes_cache = bank.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        assert_eq!(
            Bank::build_updated_stake_reward(
                distribution_epoch,
                &stake_history,
                new_warmup_cooldown_rate_epoch,
                stakes_cache_accounts,
                &partitioned_stake_reward,
                &rent,
                adjust_delegations_for_rent,
            )
            .unwrap_err(),
            DistributionError::AccountNotFound
        );
        drop(stakes_cache);

        let overflowing_account = Pubkey::new_unique();
        let mut stake_account = AccountSharedData::new(
            u64::MAX - stake_reward + 1,
            StakeStateV2::size_of(),
            &solana_stake_interface::program::id(),
        );
        stake_account
            .set_state(&StakeStateV2::Stake(
                Meta::default(),
                new_stake,
                StakeFlags::default(),
            ))
            .unwrap();
        bank.store_account(&overflowing_account, &stake_account);
        let partitioned_stake_reward = PartitionedStakeReward {
            stake_pubkey: overflowing_account,
            stake: new_stake,
            stake_reward,
            commission_bps: Some(commission_bps),
        };
        let stakes_cache = bank.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        assert_eq!(
            Bank::build_updated_stake_reward(
                distribution_epoch,
                &stake_history,
                new_warmup_cooldown_rate_epoch,
                stakes_cache_accounts,
                &partitioned_stake_reward,
                &rent,
                adjust_delegations_for_rent,
            )
            .unwrap_err(),
            DistributionError::ArithmeticOverflow
        );
        drop(stakes_cache);

        let successful_account = Pubkey::new_unique();
        let starting_stake = new_stake.delegation.stake - stake_reward;
        let starting_lamports = rent_exempt_reserve + starting_stake;
        let mut stake_account = AccountSharedData::new(
            starting_lamports,
            StakeStateV2::size_of(),
            &solana_stake_interface::program::id(),
        );
        let other_stake = Stake {
            delegation: Delegation {
                voter_pubkey,
                stake: starting_stake,
                ..Delegation::default()
            },
            credits_observed: 11,
        };
        stake_account
            .set_state(&StakeStateV2::Stake(
                Meta::default(),
                other_stake,
                StakeFlags::default(),
            ))
            .unwrap();
        bank.store_account(&successful_account, &stake_account);
        let partitioned_stake_reward = PartitionedStakeReward {
            stake_pubkey: successful_account,
            stake: new_stake,
            stake_reward,
            commission_bps: Some(commission_bps),
        };
        let stakes_cache = bank.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        let expected_lamports = starting_lamports + stake_reward;
        let mut expected_stake_account = AccountSharedData::new(
            expected_lamports,
            StakeStateV2::size_of(),
            &solana_stake_interface::program::id(),
        );
        expected_stake_account
            .set_state(&StakeStateV2::Stake(
                Meta::default(),
                new_stake,
                StakeFlags::default(),
            ))
            .unwrap();

        let expected_stake_reward = StakeReward {
            stake_pubkey: successful_account,
            stake_account: expected_stake_account,
            stake_reward_info: StakeRewardInfo {
                reward_type: RewardType::Staking,
                lamports: stake_reward as i64,
                post_balance: expected_lamports,
                commission_bps: Some(commission_bps),
            },
        };
        assert_eq!(
            Bank::build_updated_stake_reward(
                distribution_epoch,
                &stake_history,
                new_warmup_cooldown_rate_epoch,
                stakes_cache_accounts,
                &partitioned_stake_reward,
                &rent,
                adjust_delegations_for_rent,
            )
            .unwrap(),
            expected_stake_reward
        );
        drop(stakes_cache);

        let deactivating_account = Pubkey::new_unique();
        let deactivating_stake = Stake {
            delegation: Delegation {
                voter_pubkey,
                stake: 55_555,
                deactivation_epoch: bank.epoch,
                ..Delegation::default()
            },
            credits_observed: 42,
        };
        let starting_stake = deactivating_stake.delegation.stake - stake_reward;
        let starting_lamports = rent_exempt_reserve + starting_stake;
        let mut stake_account = AccountSharedData::new(
            starting_lamports,
            StakeStateV2::size_of(),
            &solana_stake_interface::program::id(),
        );
        let other_stake = Stake {
            delegation: Delegation {
                voter_pubkey,
                stake: starting_stake,
                deactivation_epoch: bank.epoch,
                ..Delegation::default()
            },
            credits_observed: 11,
        };
        stake_account
            .set_state(&StakeStateV2::Stake(
                Meta::default(),
                other_stake,
                StakeFlags::default(),
            ))
            .unwrap();
        bank.store_account(&deactivating_account, &stake_account);
        let partitioned_stake_reward = PartitionedStakeReward {
            stake_pubkey: deactivating_account,
            stake: deactivating_stake,
            stake_reward,
            commission_bps: Some(commission_bps),
        };
        let stakes_cache = bank.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        let expected_lamports = starting_lamports + stake_reward;
        let mut expected_stake_account = AccountSharedData::new(
            expected_lamports,
            StakeStateV2::size_of(),
            &solana_stake_interface::program::id(),
        );
        expected_stake_account
            .set_state(&StakeStateV2::Stake(
                Meta::default(),
                deactivating_stake,
                StakeFlags::default(),
            ))
            .unwrap();

        let expected_stake_reward = StakeReward {
            stake_pubkey: deactivating_account,
            stake_account: expected_stake_account,
            stake_reward_info: StakeRewardInfo {
                reward_type: RewardType::DeactivatedStake,
                lamports: stake_reward as i64,
                post_balance: expected_lamports,
                commission_bps: Some(commission_bps),
            },
        };
        assert_eq!(
            Bank::build_updated_stake_reward(
                distribution_epoch,
                &stake_history,
                new_warmup_cooldown_rate_epoch,
                stakes_cache_accounts,
                &partitioned_stake_reward,
                &rent,
                adjust_delegations_for_rent,
            )
            .unwrap(),
            expected_stake_reward
        );
        drop(stakes_cache);
    }

    #[test]
    fn test_update_reward_history_in_partition_empty() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let stake_rewards = vec![];

        let num_in_history = bank.update_reward_history_in_partition(&stake_rewards);
        assert_eq!(num_in_history, 0);
    }

    /// Test rewards computation and partitioned rewards distribution at the epoch boundary
    #[test]
    fn test_store_stake_accounts_in_partition() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let expected_num = 100;

        let stake_rewards = (0..expected_num)
            .map(|_| StakeReward::new_random(&bank.rent_collector.rent))
            .collect::<Vec<_>>();
        populate_starting_stake_accounts_from_stake_rewards(&bank, &stake_rewards);
        let converted_rewards = convert_rewards(stake_rewards);

        let expected_total = converted_rewards
            .enumerated_rewards_iter()
            .map(|(_, stake_reward)| stake_reward.stake_reward)
            .sum::<u64>();

        let partitioned_rewards = StartBlockHeightAndPartitionedRewards {
            distribution_starting_block_height: bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            all_stake_rewards: Arc::new(converted_rewards),
            partition_indices: vec![(0..expected_num as usize).collect::<Vec<_>>()],
        };

        let DistributionResults {
            lamports_distributed,
            ..
        } = bank.store_stake_accounts_in_partition(&partitioned_rewards, 0);
        assert_eq!(expected_total, lamports_distributed);
    }

    #[test]
    fn test_store_stake_accounts_in_partition_empty() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let partitioned_rewards = StartBlockHeightAndPartitionedRewards {
            distribution_starting_block_height: bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            all_stake_rewards: Arc::new(PartitionedStakeRewards::default()),
            partition_indices: vec![vec![]],
        };

        let expected_total = 0;

        let DistributionResults {
            lamports_distributed,
            ..
        } = bank.store_stake_accounts_in_partition(&partitioned_rewards, 0);
        assert_eq!(expected_total, lamports_distributed);
    }

    #[test]
    fn test_distribute_with_increased_rent() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(432000, 432000, false);
        let bank = Bank::new_for_tests(&genesis_config);

        // Set up epoch_rewards sysvar with rewards with 10e9 lamports to distribute.
        let total_rewards = 10 * LAMPORTS_PER_SOL;
        let num_partitions = 2; // num_partitions is arbitrary and unimportant for this test
        let total_points = (total_rewards * 42) as u128; // total_points is arbitrary for the purposes of this test
        bank.create_epoch_rewards_sysvar(
            0,
            42,
            num_partitions,
            &PointValue {
                rewards: total_rewards,
                points: total_points,
            },
        );
        let pre_epoch_rewards_account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();
        let expected_balance =
            bank.get_minimum_balance_for_rent_exemption(pre_epoch_rewards_account.data().len());
        // Expected balance is the sysvar rent-exempt balance
        assert_eq!(pre_epoch_rewards_account.lamports(), expected_balance);

        // Use lower lamports per byte for creating, bank has higher amount
        let mut lower_rent = bank.rent_collector.rent.clone();
        lower_rent.lamports_per_byte /= 10;
        let higher_rent = &bank.rent_collector.rent;

        // Set up a partition of rewards to distribute
        let stake_rewards = [
            // Zero stake -> destaked
            StakeReward::new_with_pre_stake_account(0, 0, &lower_rent),
            // Below new minimum, small reward -> destaked
            StakeReward::new_with_pre_stake_account(1, 1, &lower_rent),
            // Below new minimum, no reward -> delegation modified
            StakeReward::new_with_pre_stake_account(0, LAMPORTS_PER_SOL, &lower_rent),
            // Below new minimum, small reward -> delegation modified
            StakeReward::new_with_pre_stake_account(1, LAMPORTS_PER_SOL, &lower_rent),
            // Below new minimum, big reward -> delegation modified
            StakeReward::new_with_pre_stake_account(
                LAMPORTS_PER_SOL as i64,
                LAMPORTS_PER_SOL,
                &lower_rent,
            ),
            // Above new minimum, small reward -> delegation capped
            StakeReward::new_with_pre_stake_account(1, LAMPORTS_PER_SOL, higher_rent),
            // Above new minimum, big reward -> delegation capped
            StakeReward::new_with_pre_stake_account(
                LAMPORTS_PER_SOL as i64,
                LAMPORTS_PER_SOL,
                higher_rent,
            ),
        ]
        .into_iter()
        .map(|r| {
            bank.store_account(&r.1.stake_pubkey, &r.0);
            r.1
        })
        .collect::<Vec<_>>();

        let expected_num = stake_rewards.len();
        let rewards_to_distribute = stake_rewards
            .iter()
            .map(|stake_reward| stake_reward.stake_reward_info.lamports)
            .sum::<i64>() as u64;
        let all_rewards = convert_rewards(stake_rewards);

        let partitioned_rewards = StartBlockHeightAndPartitionedRewards {
            distribution_starting_block_height: bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            all_stake_rewards: Arc::new(all_rewards),
            partition_indices: vec![(0..expected_num).collect::<Vec<_>>()],
        };

        // Distribute rewards
        let pre_cap = bank.capitalization();
        bank.distribute_epoch_rewards_in_partition(&partitioned_rewards, 0);
        let post_cap = bank.capitalization();
        let post_epoch_rewards_account = bank.get_account(&sysvar::epoch_rewards::id()).unwrap();

        // Assert that epoch rewards sysvar lamports balance does not change
        assert_eq!(post_epoch_rewards_account.lamports(), expected_balance);

        let epoch_rewards: sysvar::epoch_rewards::EpochRewards =
            from_account(&post_epoch_rewards_account).unwrap();
        assert_eq!(epoch_rewards.total_rewards, total_rewards);
        assert_eq!(epoch_rewards.distributed_rewards, rewards_to_distribute,);

        // Assert that the bank total capital changed by the amount of rewards
        // distributed
        assert_eq!(pre_cap + rewards_to_distribute, post_cap);
    }
}
