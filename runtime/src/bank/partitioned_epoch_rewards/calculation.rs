use {
    super::{
        Bank, CalculateRewardsAndDistributeVoteRewardsResult, EpochRewardCalculateParamInfo,
        PartitionedRewardsCalculation, StakeRewardCalculationPartitioned, VoteRewardsAccounts,
    },
    crate::{
        bank::{
            PrevEpochInflationRewards, RewardCalcTracer, RewardCalculationEvent, RewardsMetrics,
            StakeRewardCalculation, VoteAccount,
        },
        epoch_rewards_hasher::hash_rewards_into_partitions,
    },
    log::info,
    rayon::{
        iter::{IntoParallelRefIterator, ParallelIterator},
        ThreadPool,
    },
    solana_measure::measure_us,
    solana_sdk::{
        clock::{Epoch, Slot},
        pubkey::Pubkey,
        reward_info::RewardInfo,
    },
    solana_stake_program::points::PointValue,
    std::sync::atomic::Ordering::Relaxed,
};

impl Bank {
    /// Begin the process of calculating and distributing rewards.
    /// This process can take multiple slots.
    pub(in crate::bank) fn begin_partitioned_rewards(
        &mut self,
        reward_calc_tracer: Option<impl Fn(&RewardCalculationEvent) + Send + Sync>,
        thread_pool: &ThreadPool,
        parent_epoch: Epoch,
        parent_slot: Slot,
        parent_block_height: u64,
        rewards_metrics: &mut RewardsMetrics,
    ) {
        let CalculateRewardsAndDistributeVoteRewardsResult {
            total_rewards,
            distributed_rewards,
            stake_rewards_by_partition,
        } = self.calculate_rewards_and_distribute_vote_rewards(
            parent_epoch,
            reward_calc_tracer,
            thread_pool,
            rewards_metrics,
        );

        let slot = self.slot();
        let credit_start = self.block_height() + self.get_reward_calculation_num_blocks();

        self.set_epoch_reward_status_active(stake_rewards_by_partition);

        // create EpochRewards sysvar that holds the balance of undistributed rewards with
        // (total_rewards, distributed_rewards, credit_start), total capital will increase by (total_rewards - distributed_rewards)
        self.create_epoch_rewards_sysvar(total_rewards, distributed_rewards, credit_start);

        datapoint_info!(
            "epoch-rewards-status-update",
            ("start_slot", slot, i64),
            ("start_block_height", self.block_height(), i64),
            ("active", 1, i64),
            ("parent_slot", parent_slot, i64),
            ("parent_block_height", parent_block_height, i64),
        );
    }

    // Calculate rewards from previous epoch and distribute vote rewards
    fn calculate_rewards_and_distribute_vote_rewards(
        &self,
        prev_epoch: Epoch,
        reward_calc_tracer: Option<impl Fn(&RewardCalculationEvent) + Send + Sync>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> CalculateRewardsAndDistributeVoteRewardsResult {
        let PartitionedRewardsCalculation {
            vote_account_rewards,
            stake_rewards_by_partition,
            old_vote_balance_and_staked,
            validator_rewards,
            validator_rate,
            foundation_rate,
            prev_epoch_duration_in_years,
            capitalization,
        } = self.calculate_rewards_for_partitioning(
            prev_epoch,
            reward_calc_tracer,
            thread_pool,
            metrics,
        );
        let vote_rewards = self.store_vote_accounts_partitioned(vote_account_rewards, metrics);

        // update reward history of JUST vote_rewards, stake_rewards is vec![] here
        self.update_reward_history(vec![], vote_rewards);

        let StakeRewardCalculationPartitioned {
            stake_rewards_by_partition,
            total_stake_rewards_lamports,
        } = stake_rewards_by_partition;

        // the remaining code mirrors `update_rewards_with_thread_pool()`

        let new_vote_balance_and_staked = self.stakes_cache.stakes().vote_balance_and_staked();

        // This is for vote rewards only.
        let validator_rewards_paid = new_vote_balance_and_staked - old_vote_balance_and_staked;
        self.assert_validator_rewards_paid(validator_rewards_paid);

        // verify that we didn't pay any more than we expected to
        assert!(validator_rewards >= validator_rewards_paid + total_stake_rewards_lamports);

        info!(
            "distributed vote rewards: {} out of {}, remaining {}",
            validator_rewards_paid, validator_rewards, total_stake_rewards_lamports
        );

        let (num_stake_accounts, num_vote_accounts) = {
            let stakes = self.stakes_cache.stakes();
            (
                stakes.stake_delegations().len(),
                stakes.vote_accounts().len(),
            )
        };
        self.capitalization
            .fetch_add(validator_rewards_paid, Relaxed);

        let active_stake = if let Some(stake_history_entry) =
            self.stakes_cache.stakes().history().get(prev_epoch)
        {
            stake_history_entry.effective
        } else {
            0
        };

        datapoint_info!(
            "epoch_rewards",
            ("slot", self.slot, i64),
            ("epoch", prev_epoch, i64),
            ("validator_rate", validator_rate, f64),
            ("foundation_rate", foundation_rate, f64),
            ("epoch_duration_in_years", prev_epoch_duration_in_years, f64),
            ("validator_rewards", validator_rewards_paid, i64),
            ("active_stake", active_stake, i64),
            ("pre_capitalization", capitalization, i64),
            ("post_capitalization", self.capitalization(), i64),
            ("num_stake_accounts", num_stake_accounts, i64),
            ("num_vote_accounts", num_vote_accounts, i64),
        );

        CalculateRewardsAndDistributeVoteRewardsResult {
            total_rewards: validator_rewards_paid + total_stake_rewards_lamports,
            distributed_rewards: validator_rewards_paid,
            stake_rewards_by_partition,
        }
    }

    pub(in crate::bank) fn store_vote_accounts_partitioned(
        &self,
        vote_account_rewards: VoteRewardsAccounts,
        metrics: &RewardsMetrics,
    ) -> Vec<(Pubkey, RewardInfo)> {
        let (_, measure_us) = measure_us!({
            // reformat data to make it not sparse.
            // `StorableAccounts` does not efficiently handle sparse data.
            // Not all entries in `vote_account_rewards.accounts_to_store` have a Some(account) to store.
            let to_store = vote_account_rewards
                .accounts_to_store
                .iter()
                .filter_map(|account| account.as_ref())
                .enumerate()
                .map(|(i, account)| (&vote_account_rewards.rewards[i].0, account))
                .collect::<Vec<_>>();
            self.store_accounts((self.slot(), &to_store[..]));
        });

        metrics
            .store_vote_accounts_us
            .fetch_add(measure_us, Relaxed);

        vote_account_rewards.rewards
    }

    /// Calculate rewards from previous epoch to prepare for partitioned distribution.
    pub(in crate::bank) fn calculate_rewards_for_partitioning(
        &self,
        prev_epoch: Epoch,
        reward_calc_tracer: Option<impl Fn(&RewardCalculationEvent) + Send + Sync>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> PartitionedRewardsCalculation {
        let capitalization = self.capitalization();
        let PrevEpochInflationRewards {
            validator_rewards,
            prev_epoch_duration_in_years,
            validator_rate,
            foundation_rate,
        } = self.calculate_previous_epoch_inflation_rewards(capitalization, prev_epoch);

        let old_vote_balance_and_staked = self.stakes_cache.stakes().vote_balance_and_staked();

        let (vote_account_rewards, mut stake_rewards) = self
            .calculate_validator_rewards(
                prev_epoch,
                validator_rewards,
                reward_calc_tracer,
                thread_pool,
                metrics,
            )
            .unwrap_or_default();

        let num_partitions = self.get_reward_distribution_num_blocks(&stake_rewards.stake_rewards);
        let parent_blockhash = self
            .parent()
            .expect("Partitioned rewards calculation must still have access to parent Bank.")
            .last_blockhash();
        let stake_rewards_by_partition = hash_rewards_into_partitions(
            std::mem::take(&mut stake_rewards.stake_rewards),
            &parent_blockhash,
            num_partitions as usize,
        );

        PartitionedRewardsCalculation {
            vote_account_rewards,
            stake_rewards_by_partition: StakeRewardCalculationPartitioned {
                stake_rewards_by_partition,
                total_stake_rewards_lamports: stake_rewards.total_stake_rewards_lamports,
            },
            old_vote_balance_and_staked,
            validator_rewards,
            validator_rate,
            foundation_rate,
            prev_epoch_duration_in_years,
            capitalization,
        }
    }

    /// Calculate epoch reward and return vote and stake rewards.
    pub(in crate::bank) fn calculate_validator_rewards(
        &self,
        rewarded_epoch: Epoch,
        rewards: u64,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> Option<(VoteRewardsAccounts, StakeRewardCalculation)> {
        let stakes = self.stakes_cache.stakes();
        let reward_calculate_param = self.get_epoch_reward_calculate_param_info(&stakes);

        self.calculate_reward_points_partitioned(
            &reward_calculate_param,
            rewards,
            thread_pool,
            metrics,
        )
        .map(|point_value| {
            self.calculate_stake_vote_rewards(
                &reward_calculate_param,
                rewarded_epoch,
                point_value,
                thread_pool,
                reward_calc_tracer,
                metrics,
            )
        })
    }

    /// Calculates epoch reward points from stake/vote accounts.
    /// Returns reward lamports and points for the epoch or none if points == 0.
    pub(in crate::bank) fn calculate_reward_points_partitioned(
        &self,
        reward_calculate_params: &EpochRewardCalculateParamInfo,
        rewards: u64,
        thread_pool: &ThreadPool,
        metrics: &RewardsMetrics,
    ) -> Option<PointValue> {
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = reward_calculate_params;

        let solana_vote_program: Pubkey = solana_vote_program::id();

        let get_vote_account = |vote_pubkey: &Pubkey| -> Option<VoteAccount> {
            if let Some(vote_account) = cached_vote_accounts.get(vote_pubkey) {
                return Some(vote_account.clone());
            }
            // If accounts-db contains a valid vote account, then it should
            // already have been cached in cached_vote_accounts; so the code
            // below is only for sanity checking, and can be removed once
            // the cache is deemed to be reliable.
            let account = self.get_account_with_fixed_root(vote_pubkey)?;
            VoteAccount::try_from(account).ok()
        };

        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        let (points, measure_us) = measure_us!(thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .map(|(_stake_pubkey, stake_account)| {
                    let delegation = stake_account.delegation();
                    let vote_pubkey = delegation.voter_pubkey;

                    let Some(vote_account) = get_vote_account(&vote_pubkey) else {
                        return 0;
                    };
                    if vote_account.owner() != &solana_vote_program {
                        return 0;
                    }
                    let Ok(vote_state) = vote_account.vote_state() else {
                        return 0;
                    };

                    solana_stake_program::points::calculate_points(
                        stake_account.stake_state(),
                        vote_state,
                        stake_history,
                        new_warmup_cooldown_rate_epoch,
                    )
                    .unwrap_or(0)
                })
                .sum::<u128>()
        }));
        metrics.calculate_points_us.fetch_add(measure_us, Relaxed);

        (points > 0).then_some(PointValue { rewards, points })
    }
}
