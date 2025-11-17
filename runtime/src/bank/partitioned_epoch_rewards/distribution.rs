use {
    super::{
        epoch_rewards_hasher, Bank, EpochRewardStatus, PartitionedStakeReward, StakeRewards,
        StartBlockHeightAndPartitionedRewards,
    },
    crate::{
        bank::{
            metrics::{report_partitioned_reward_metrics, RewardsStoreMetrics},
            partitioned_epoch_rewards::EpochRewardPhase,
        },
        stake_account::StakeAccount,
    },
    log::error,
    serde::{Deserialize, Serialize},
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount, WritableAccount},
    solana_accounts_db::stake_rewards::StakeReward,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_reward_info::{RewardInfo, RewardType},
    solana_stake_interface::state::{Delegation, StakeStateV2},
    std::sync::{atomic::Ordering::Relaxed, Arc},
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
            .for_each(|x| rewards.push((x.stake_pubkey, x.stake_reward_info)));
        rewards.len().saturating_sub(initial_len)
    }

    fn build_updated_stake_reward(
        stakes_cache_accounts: &im::HashMap<Pubkey, StakeAccount<Delegation>>,
        partitioned_stake_reward: &PartitionedStakeReward,
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
        assert_eq!(
            stake
                .delegation
                .stake
                .saturating_add(partitioned_stake_reward.stake_reward),
            partitioned_stake_reward.stake.delegation.stake,
        );
        account
            .set_state(&StakeStateV2::Stake(
                meta,
                partitioned_stake_reward.stake,
                flags,
            ))
            .map_err(|_| DistributionError::UnableToSetState)?;
        Ok(StakeReward {
            stake_pubkey: partitioned_stake_reward.stake_pubkey,
            stake_reward_info: RewardInfo {
                reward_type: RewardType::Staking,
                lamports: i64::try_from(partitioned_stake_reward.stake_reward).unwrap(),
                post_balance: account.lamports(),
                commission: Some(partitioned_stake_reward.commission),
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
            match Self::build_updated_stake_reward(stakes_cache_accounts, partitioned_stake_reward)
            {
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
                    epoch_rewards_hasher::hash_rewards_into_partitions, tests::convert_rewards,
                    PartitionedStakeRewards, REWARD_CALCULATION_NUM_BLOCKS,
                },
                tests::create_genesis_config,
            },
            inflation_rewards::points::PointValue,
            stake_utils,
        },
        rand::Rng,
        solana_account::from_account,
        solana_accounts_db::stake_rewards::StakeReward,
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_rent::Rent,
        solana_reward_info::{RewardInfo, RewardType},
        solana_stake_interface::{
            stake_flags::StakeFlags,
            state::{Meta, Stake},
        },
        solana_sysvar as sysvar,
        solana_vote_program::vote_state,
        std::sync::Arc,
    };

    #[test]
    fn test_distribute_partitioned_epoch_rewards() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let mut bank = Bank::new_for_tests(&genesis_config);

        let expected_num = 100;

        let stake_rewards = (0..expected_num)
            .map(|_| Some(PartitionedStakeReward::new_random()))
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
            .map(|_| Some(PartitionedStakeReward::new_random()))
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
        let rent = Rent::free();
        let validator_pubkey = Pubkey::new_unique();
        let validator_vote_pubkey = Pubkey::new_unique();

        let validator_vote_account = vote_state::create_v4_account_with_authorized(
            &validator_pubkey,
            &validator_vote_pubkey,
            &validator_vote_pubkey,
            None,
            1000,
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
                &rent,
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
            .map(|_| StakeReward::new_random())
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
            .map(|_| StakeReward::new_random())
            .collect::<Vec<_>>();
        populate_starting_stake_accounts_from_stake_rewards(&bank, &stake_rewards);

        let expected_rewards = stake_rewards
            .iter()
            .map(|stake_reward| stake_reward.stake_reward_info.lamports)
            .sum::<i64>() as u64;

        // Push extra StakeReward to simulate non-existent account
        stake_rewards.push(StakeReward::new_random());

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
                .map(|_| StakeReward::new_random())
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
                            &expected_stake_reward.stake_reward_info
                        ),
                        (k, reward_info)
                    );
                });

            assert_eq!(num_in_history, expected_num);
        }
    }

    #[test]
    fn test_build_updated_stake_reward() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

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
        let commission = 42;

        let nonexistent_account = Pubkey::new_unique();
        let partitioned_stake_reward = PartitionedStakeReward {
            stake_pubkey: nonexistent_account,
            stake: new_stake,
            stake_reward,
            commission,
        };
        let stakes_cache = bank.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        assert_eq!(
            Bank::build_updated_stake_reward(stakes_cache_accounts, &partitioned_stake_reward)
                .unwrap_err(),
            DistributionError::AccountNotFound
        );
        drop(stakes_cache);

        let rent_exempt_reserve = 2_282_880;

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
            commission,
        };
        let stakes_cache = bank.stakes_cache.stakes();
        let stakes_cache_accounts = stakes_cache.stake_delegations();
        assert_eq!(
            Bank::build_updated_stake_reward(stakes_cache_accounts, &partitioned_stake_reward)
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
            commission,
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
            stake_reward_info: RewardInfo {
                reward_type: RewardType::Staking,
                lamports: stake_reward as i64,
                post_balance: expected_lamports,
                commission: Some(commission),
            },
        };
        assert_eq!(
            Bank::build_updated_stake_reward(stakes_cache_accounts, &partitioned_stake_reward)
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
            .map(|_| StakeReward::new_random())
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
}
