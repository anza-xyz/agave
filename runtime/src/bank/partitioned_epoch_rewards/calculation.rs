use {
    super::{
        epoch_rewards_hasher::hash_rewards_into_partitions, Bank, CachedVoteAccounts,
        CalculateValidatorRewardsResult, EpochRewardCalculateParamInfo, FilteredStakeDelegations,
        PartitionedRewardsCalculation, PartitionedStakeReward, PartitionedStakeRewards,
        StakeRewardCalculation, VoteRewardsAccounts, VoteRewardsAccountsStorable,
        REWARD_CALCULATION_NUM_BLOCKS,
    },
    crate::{
        bank::{
            null_tracer, PrevEpochInflationRewards, RewardCalcTracer, RewardCalculationEvent,
            RewardsMetrics, VoteReward, VoteRewards,
        },
        inflation_rewards::{
            points::{calculate_points, DelegatedVoteState, PointValue},
            redeem_rewards,
        },
        stake_account::StakeAccount,
        stake_utils,
        stakes::Stakes,
    },
    agave_feature_set as feature_set,
    log::{debug, info},
    rayon::{
        iter::{IndexedParallelIterator, ParallelIterator},
        ThreadPool,
    },
    solana_clock::{Epoch, Slot},
    solana_measure::{measure::Measure, measure_us},
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    solana_stake_interface::{stake_history::StakeHistory, state::Delegation},
    solana_sysvar::epoch_rewards::EpochRewards,
    std::sync::{atomic::Ordering::Relaxed, Arc},
};

#[derive(Debug)]
struct DelegationRewards {
    stake_reward: PartitionedStakeReward,
    vote_pubkey: Pubkey,
    vote_reward: VoteReward,
}

#[derive(Default)]
struct RewardsAccumulator {
    vote_rewards: VoteRewards,
    num_stake_rewards: usize,
    total_stake_rewards_lamports: u64,
}

impl RewardsAccumulator {
    fn add_reward(&mut self, vote_pubkey: Pubkey, vote_reward: VoteReward, stakers_reward: u64) {
        self.vote_rewards
            .entry(vote_pubkey)
            .and_modify(|dst_vote_reward| {
                dst_vote_reward.vote_rewards = dst_vote_reward
                    .vote_rewards
                    .saturating_add(vote_reward.vote_rewards)
            })
            .or_insert(vote_reward);
        self.num_stake_rewards = self.num_stake_rewards.saturating_add(1);
        self.total_stake_rewards_lamports = self
            .total_stake_rewards_lamports
            .saturating_add(stakers_reward);
    }

    /// Merges two instances by combining their vote rewards and stake rewards.
    ///
    /// To minimize reallocations, the instance with more vote rewards is used
    /// as the base and the smaller instance is merged into it.
    fn accumulate_into_larger(self, rhs: Self) -> Self {
        // Check which instance has more vote rewards. Treat the bigger one
        // as a destination, which is going to be extended. This way we make
        // the reallocation as small as possible.
        let (mut dst, src) = if self.vote_rewards.len() >= rhs.vote_rewards.len() {
            (self, rhs)
        } else {
            (rhs, self)
        };
        for (vote_pubkey, vote_reward) in src.vote_rewards {
            dst.vote_rewards
                .entry(vote_pubkey)
                .and_modify(|dst_vote_reward: &mut VoteReward| {
                    dst_vote_reward.vote_rewards = dst_vote_reward
                        .vote_rewards
                        .saturating_add(vote_reward.vote_rewards)
                })
                .or_insert(vote_reward);
        }
        dst.num_stake_rewards = dst.num_stake_rewards.saturating_add(src.num_stake_rewards);
        dst.total_stake_rewards_lamports = dst
            .total_stake_rewards_lamports
            .saturating_add(src.total_stake_rewards_lamports);
        dst
    }
}

impl Bank {
    /// Begin the process of calculating and distributing rewards.
    /// This process can take multiple slots.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::bank) fn begin_partitioned_rewards(
        &mut self,
        parent_epoch: Epoch,
        parent_slot: Slot,
        parent_block_height: u64,
        rewards_calculation: &PartitionedRewardsCalculation,
        rewards_metrics: &RewardsMetrics,
    ) {
        self.distribute_vote_rewards(parent_epoch, rewards_calculation, rewards_metrics);

        let slot = self.slot();
        let distribution_starting_block_height =
            self.block_height() + REWARD_CALCULATION_NUM_BLOCKS;

        let PartitionedRewardsCalculation {
            vote_account_rewards,
            stake_rewards,
            point_value,
            ..
        } = rewards_calculation;

        let distributed_rewards = vote_account_rewards.total_vote_rewards_lamports;
        let stake_rewards = Arc::clone(&stake_rewards.stake_rewards);

        let num_partitions = self.get_reward_distribution_num_blocks(&stake_rewards);

        self.set_epoch_reward_status_calculation(distribution_starting_block_height, stake_rewards);

        self.create_epoch_rewards_sysvar(
            distributed_rewards,
            distribution_starting_block_height,
            num_partitions,
            point_value,
        );

        datapoint_info!(
            "epoch-rewards-status-update",
            ("start_slot", slot, i64),
            ("calculation_block_height", self.block_height(), i64),
            ("active", 1, i64),
            ("parent_slot", parent_slot, i64),
            ("parent_block_height", parent_block_height, i64),
        );
    }

    // Calculate rewards from previous epoch and distribute vote rewards
    pub(in crate::bank) fn calculate_rewards(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: Vec<(&Pubkey, &StakeAccount<Delegation>)>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        prev_epoch: Epoch,
        reward_calc_tracer: Option<impl Fn(&RewardCalculationEvent) + Send + Sync>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> Arc<PartitionedRewardsCalculation> {
        // We hold the lock here for the epoch rewards calculation cache to prevent
        // rewards computation across multiple forks simultaneously. This aligns with
        // how banks are currently created- all banks are created sequentially.
        // As such, this lock does not actually introduce contention because bank
        // creation (and therefore reward calculation) is always done sequentially.
        //
        // However, if we plan to support creating banks in parallel in the future, this logic
        // would need to change to allow rewards computation on multiple forks concurrently.
        // That said, there's still a compelling reason to keep this lock even in a parallel
        // bank creation model: we want to avoid calculating rewards multiple times for the same
        // parent bank hash. This lock ensures that.
        //
        // Creating bank for multiple forks in parallel would also introduce contention for compute resources,
        // potentially slowing down the performance of both forks. This, in turn, could delay
        // vote propagation and consensus for the leading fork—the one most likely to become rooted.
        //
        // Therefore, it seems beneficial to continue processing forks sequentially at epoch
        // boundaries: acquire the lock for the first fork, compute rewards, and let other forks
        // wait until the computation is complete.
        let mut epoch_rewards_calculation_cache =
            self.epoch_rewards_calculation_cache.lock().unwrap();
        let rewards_calculation = epoch_rewards_calculation_cache
            .entry(self.parent_hash)
            .or_insert_with(|| {
                let stake_delegations = self.filter_stake_delegations(stake_delegations);
                Arc::new(self.calculate_rewards_for_partitioning(
                    stake_history,
                    &stake_delegations,
                    cached_vote_accounts,
                    prev_epoch,
                    reward_calc_tracer,
                    thread_pool,
                    metrics,
                ))
            })
            .clone();
        drop(epoch_rewards_calculation_cache);

        rewards_calculation
    }

    pub(in crate::bank) fn distribute_vote_rewards(
        &mut self,
        prev_epoch: Epoch,
        rewards_calculation: &PartitionedRewardsCalculation,
        rewards_metrics: &RewardsMetrics,
    ) {
        let PartitionedRewardsCalculation {
            vote_account_rewards,
            stake_rewards,
            validator_rate,
            foundation_rate,
            prev_epoch_duration_in_years,
            capitalization,
            point_value,
            ..
        } = rewards_calculation;

        let total_vote_rewards = vote_account_rewards.total_vote_rewards_lamports;
        self.store_vote_accounts_partitioned(vote_account_rewards, rewards_metrics);
        self.update_vote_rewards(vote_account_rewards);

        let StakeRewardCalculation {
            total_stake_rewards_lamports,
            ..
        } = stake_rewards;

        // verify that we didn't pay any more than we expected to
        assert!(point_value.rewards >= total_vote_rewards + total_stake_rewards_lamports);
        info!(
            "distributed vote rewards: {} out of {}, remaining {}",
            total_vote_rewards, point_value.rewards, total_stake_rewards_lamports
        );

        let (num_stake_accounts, num_vote_accounts) = {
            let stakes = self.stakes_cache.stakes();
            (
                stakes.stake_delegations().len(),
                stakes.vote_accounts().len(),
            )
        };
        self.capitalization.fetch_add(total_vote_rewards, Relaxed);

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
            ("validator_rate", *validator_rate, f64),
            ("foundation_rate", *foundation_rate, f64),
            (
                "epoch_duration_in_years",
                *prev_epoch_duration_in_years,
                f64
            ),
            ("validator_rewards", total_vote_rewards, i64),
            ("active_stake", active_stake, i64),
            ("pre_capitalization", *capitalization, i64),
            ("post_capitalization", self.capitalization(), i64),
            ("num_stake_accounts", num_stake_accounts, i64),
            ("num_vote_accounts", num_vote_accounts, i64),
        );
    }

    fn store_vote_accounts_partitioned(
        &self,
        vote_account_rewards: &VoteRewardsAccounts,
        metrics: &RewardsMetrics,
    ) {
        let (_, measure_us) = measure_us!({
            let storable = VoteRewardsAccountsStorable {
                slot: self.slot(),
                vote_rewards_accounts: vote_account_rewards,
            };
            self.store_accounts(storable);
        });

        metrics
            .store_vote_accounts_us
            .fetch_add(measure_us, Relaxed);
    }

    /// Calculate rewards from previous epoch to prepare for partitioned distribution.
    pub(super) fn calculate_rewards_for_partitioning<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: &'a FilteredStakeDelegations<'a>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
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
        } = self.calculate_previous_epoch_inflation_rewards(capitalization, rewarded_epoch);

        let CalculateValidatorRewardsResult {
            vote_rewards_accounts: vote_account_rewards,
            stake_reward_calculation: stake_rewards,
            point_value,
        } = self
            .calculate_validator_rewards(
                stake_history,
                stake_delegations,
                cached_vote_accounts,
                rewarded_epoch,
                validator_rewards,
                reward_calc_tracer,
                thread_pool,
                metrics,
            )
            .unwrap_or_default();

        info!(
            "calculated rewards for epoch: {}, parent_slot: {}, parent_hash: {}",
            self.epoch, self.parent_slot, self.parent_hash
        );

        PartitionedRewardsCalculation {
            vote_account_rewards,
            stake_rewards,
            validator_rate,
            foundation_rate,
            prev_epoch_duration_in_years,
            capitalization,
            point_value,
        }
    }

    /// Calculate epoch reward and return vote and stake rewards.
    fn calculate_validator_rewards<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: &'a FilteredStakeDelegations<'a>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
        rewards: u64,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> Option<CalculateValidatorRewardsResult> {
        self.calculate_reward_points_partitioned(
            stake_history,
            stake_delegations,
            &cached_vote_accounts,
            rewards,
            thread_pool,
            metrics,
        )
        .map(|point_value| {
            let (vote_rewards_accounts, stake_reward_calculation) = self
                .calculate_stake_vote_rewards(
                    stake_history,
                    stake_delegations,
                    cached_vote_accounts,
                    rewarded_epoch,
                    point_value.clone(),
                    thread_pool,
                    reward_calc_tracer,
                    metrics,
                );
            CalculateValidatorRewardsResult {
                vote_rewards_accounts,
                stake_reward_calculation,
                point_value,
            }
        })
    }

    pub(in crate::bank) fn filter_stake_delegations<'a>(
        &self,
        stake_delegations: Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
    ) -> FilteredStakeDelegations<'a> {
        let min_stake_delegation = if self
            .feature_set
            .is_active(&feature_set::stake_minimum_delegation_for_rewards::id())
        {
            let min_stake_delegation = stake_utils::get_minimum_delegation(
                self.feature_set
                    .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
            )
            .max(LAMPORTS_PER_SOL);
            Some(min_stake_delegation)
        } else {
            None
        };
        FilteredStakeDelegations {
            stake_delegations,
            min_stake_delegation,
        }
    }

    /// Retrieves stake history and delegations for stake reward recalculation
    /// after snapshot restore.
    fn get_epoch_params_for_recalculation<'a>(
        &'a self,
        rewarded_epoch: Epoch,
        stakes: &'a Stakes<StakeAccount<Delegation>>,
    ) -> EpochRewardCalculateParamInfo<'a> {
        // Use `stakes` for stake-related info
        let stake_history = stakes.history().clone();
        let stake_delegations = stakes.stake_delegations_vec();
        let stake_delegations = self.filter_stake_delegations(stake_delegations);

        // Vote account state from the end of the rewarded epoch / beginning of the
        // distribution epoch.
        let leader_schedule_epoch = self.epoch_schedule().get_leader_schedule_epoch(self.slot());
        let distribution_epoch_vote_accounts = self
            .epoch_stakes(leader_schedule_epoch)
            .expect("calculation should always run after Bank::update_epoch_stakes()")
            .stakes()
            .vote_accounts();
        let cached_vote_accounts =
            self.get_cached_vote_accounts(rewarded_epoch, distribution_epoch_vote_accounts);

        EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn redeem_delegation_rewards(
        &self,
        rewarded_epoch: Epoch,
        stake_pubkey: &Pubkey,
        stake_account: &StakeAccount<Delegation>,
        point_value: &PointValue,
        stake_history: &StakeHistory,
        cached_vote_accounts: &CachedVoteAccounts<'_>,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        new_rate_activation_epoch: Option<Epoch>,
        delay_commission_updates: bool,
    ) -> Option<DelegationRewards> {
        // curry closure to add the contextual stake_pubkey
        let reward_calc_tracer = reward_calc_tracer.as_ref().map(|outer| {
            // inner
            move |inner_event: &_| {
                outer(&RewardCalculationEvent::Staking(stake_pubkey, inner_event))
            }
        });

        let CachedVoteAccounts {
            snapshot_epoch_vote_accounts,
            rewarded_epoch_vote_accounts,
            distribution_epoch_vote_accounts,
        } = cached_vote_accounts;

        let stake_pubkey = *stake_pubkey;
        let vote_pubkey = stake_account.delegation().voter_pubkey;
        let Some(vote_account) = distribution_epoch_vote_accounts.get(&vote_pubkey) else {
            debug!("could not find vote account {vote_pubkey} in cache");
            return None;
        };
        let vote_state = vote_account.vote_state_view();
        let stake_state = stake_account.stake_state();

        // Fetch the voter commission from past epochs to attempt to
        // delay the effect of commission updates by at least one
        // full epoch
        let commission = if delay_commission_updates {
            snapshot_epoch_vote_accounts
                .and_then(|eva| eva.get(&vote_pubkey))
                .or_else(|| rewarded_epoch_vote_accounts.and_then(|eva| eva.get(&vote_pubkey)))
                .map(|vote_account| vote_account.vote_state_view())
                .unwrap_or(vote_state)
                .commission()
        } else {
            vote_state.commission()
        };

        match redeem_rewards(
            rewarded_epoch,
            stake_state,
            commission,
            DelegatedVoteState::from(vote_state),
            point_value,
            stake_history,
            reward_calc_tracer,
            new_rate_activation_epoch,
        ) {
            Ok((stake_reward, vote_rewards, stake)) => {
                let stake_reward = PartitionedStakeReward {
                    stake_pubkey,
                    stake,
                    stake_reward,
                    commission,
                };
                let vote_account = vote_account.into();
                let vote_reward = VoteReward {
                    commission,
                    vote_account,
                    vote_rewards,
                };
                Some(DelegationRewards {
                    stake_reward,
                    vote_pubkey,
                    vote_reward,
                })
            }
            Err(e) => {
                debug!("redeem_rewards() failed for {stake_pubkey}: {e:?}");
                None
            }
        }
    }

    /// Calculates epoch rewards for stake/vote accounts
    /// Returns vote rewards, stake rewards, and the sum of all stake rewards in lamports
    fn calculate_stake_vote_rewards<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: &'a FilteredStakeDelegations<'a>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
        point_value: PointValue,
        thread_pool: &ThreadPool,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        metrics: &mut RewardsMetrics,
    ) -> (VoteRewardsAccounts, StakeRewardCalculation) {
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        let delay_commission_updates = self
            .feature_set
            .is_active(&agave_feature_set::delay_commission_updates::id());

        let mut measure_redeem_rewards = Measure::start("redeem-rewards");
        // For N stake delegations, where N is >1,000,000, we produce:
        // * N stake rewards,
        // * M vote rewards, where M is a number of stake nodes. Currently, way
        //   smaller number than 1,000,000. And we can expect it to always be
        //   significantly smaller than number of delegations.
        //
        // Producing the stake reward with rayon triggers a lot of
        // (re)allocations. To avoid that, we allocate it at the start and
        // pass `stake_rewards.spare_capacity_mut()` as one of iterators.
        let mut stake_rewards = PartitionedStakeRewards::with_capacity(stake_delegations.len());
        let rewards_accumulator: RewardsAccumulator = thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .zip_eq(stake_rewards.spare_capacity_mut())
                .with_min_len(500)
                .filter_map(|(maybe_stake_delegation, stake_reward_ref)| {
                    let maybe_reward_record =
                        maybe_stake_delegation.and_then(|(stake_pubkey, stake_account)| {
                            self.redeem_delegation_rewards(
                                rewarded_epoch,
                                stake_pubkey,
                                stake_account,
                                &point_value,
                                stake_history,
                                &cached_vote_accounts,
                                reward_calc_tracer.as_ref(),
                                new_warmup_cooldown_rate_epoch,
                                delay_commission_updates,
                            )
                        });
                    let (stake_reward, maybe_reward_record) = match maybe_reward_record {
                        Some(res) => {
                            let DelegationRewards {
                                stake_reward,
                                vote_pubkey,
                                vote_reward,
                            } = res;
                            let stakers_reward = stake_reward.stake_reward;
                            (
                                Some(stake_reward),
                                Some((stakers_reward, vote_pubkey, vote_reward)),
                            )
                        }
                        None => (None, None),
                    };
                    // It's important that for every stake delegation, we write
                    // a value to the cell of the stake rewards vector,
                    // regardless of whether it's `Some` or `None` variant.
                    // This allows us to pre-allocate the vector with the known
                    // size and avoid re-allocations, which were the bottleneck
                    // in this path.
                    stake_reward_ref.write(stake_reward);
                    maybe_reward_record
                })
                .fold(
                    RewardsAccumulator::default,
                    |mut rewards_accumulator, (stake_reward, vote_pubkey, vote_reward)| {
                        rewards_accumulator.add_reward(vote_pubkey, vote_reward, stake_reward);
                        rewards_accumulator
                    },
                )
                .reduce(
                    RewardsAccumulator::default,
                    |rewards_accumulator_a, rewards_accumulator_b| {
                        rewards_accumulator_a.accumulate_into_larger(rewards_accumulator_b)
                    },
                )
        });
        let RewardsAccumulator {
            vote_rewards,
            num_stake_rewards,
            total_stake_rewards_lamports,
        } = rewards_accumulator;
        // SAFETY: We initialized all the `stake_rewards` elements up to the capacity.
        unsafe {
            stake_rewards.assume_init(num_stake_rewards);
        }
        let vote_rewards = Self::calc_vote_accounts_to_store(vote_rewards);
        measure_redeem_rewards.stop();
        metrics.redeem_rewards_us = measure_redeem_rewards.as_us();

        (
            vote_rewards,
            StakeRewardCalculation {
                stake_rewards: Arc::new(stake_rewards),
                total_stake_rewards_lamports,
            },
        )
    }

    /// Calculates epoch reward points from stake/vote accounts.
    /// Returns reward lamports and points for the epoch or none if points == 0.
    fn calculate_reward_points_partitioned<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: &'a FilteredStakeDelegations<'a>,
        cached_vote_accounts: &CachedVoteAccounts<'_>,
        rewards: u64,
        thread_pool: &ThreadPool,
        metrics: &RewardsMetrics,
    ) -> Option<PointValue> {
        let CachedVoteAccounts {
            distribution_epoch_vote_accounts,
            ..
        } = cached_vote_accounts;

        let solana_vote_program: Pubkey = solana_vote_program::id();
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        let (points, measure_us) = measure_us!(thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .filter_map(|stake_delegation| stake_delegation)
                .map(|(_stake_pubkey, stake_account)| {
                    let vote_pubkey = stake_account.delegation().voter_pubkey;

                    let Some(vote_account) = distribution_epoch_vote_accounts.get(&vote_pubkey)
                    else {
                        return 0;
                    };
                    if vote_account.owner() != &solana_vote_program {
                        return 0;
                    }

                    calculate_points(
                        stake_account.stake_state(),
                        DelegatedVoteState::from(vote_account.vote_state_view()),
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

    /// If rewards are still active, recalculates partitioned stake rewards and
    /// updates Bank::epoch_reward_status. This method assumes that vote rewards
    /// have already been calculated and delivered, and *only* recalculates
    /// stake rewards
    pub(in crate::bank) fn recalculate_partitioned_rewards_if_active<F, TP>(
        &mut self,
        thread_pool_builder: F,
    ) where
        F: FnOnce() -> TP,
        TP: std::borrow::Borrow<ThreadPool>,
    {
        let epoch_rewards_sysvar = self.get_epoch_rewards_sysvar();
        if epoch_rewards_sysvar.active {
            let thread_pool = thread_pool_builder();
            let (_, stake_rewards, partition_indices) =
                self.recalculate_stake_rewards(&epoch_rewards_sysvar, thread_pool.borrow());
            self.set_epoch_reward_status_distribution(
                epoch_rewards_sysvar.distribution_starting_block_height,
                stake_rewards,
                partition_indices,
            );
        }
    }

    /// Returns a vector of partitioned stake rewards. StakeRewards are
    /// recalculated from an active EpochRewards sysvar, vote accounts from
    /// EpochStakes, and stake accounts from StakesCache.
    fn recalculate_stake_rewards(
        &self,
        epoch_rewards_sysvar: &EpochRewards,
        thread_pool: &ThreadPool,
    ) -> (
        VoteRewardsAccounts,
        Arc<PartitionedStakeRewards>,
        Vec<Vec<usize>>,
    ) {
        assert!(epoch_rewards_sysvar.active);
        // If rewards are active, the rewarded epoch is always the immediately
        // preceding epoch.
        let rewarded_epoch = self.epoch().saturating_sub(1);

        let point_value = PointValue {
            rewards: epoch_rewards_sysvar.total_rewards,
            points: epoch_rewards_sysvar.total_points,
        };

        let stakes = self.stakes_cache.stakes();
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = self.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);

        // On recalculation, only the `StakeRewardCalculation::stake_rewards`
        // field is relevant. It is assumed that vote-account rewards have
        // already been calculated and delivered, while
        // `StakeRewardCalculation::total_rewards` only reflects rewards that
        // have not yet been distributed.
        let (vote_rewards_accounts, StakeRewardCalculation { stake_rewards, .. }) = self
            .calculate_stake_vote_rewards(
                &stake_history,
                &stake_delegations,
                cached_vote_accounts,
                rewarded_epoch,
                point_value,
                thread_pool,
                null_tracer(),
                &mut RewardsMetrics::default(), // This is required, but not reporting anything at the moment
            );
        drop(stakes);
        let partition_indices = hash_rewards_into_partitions(
            &stake_rewards,
            &epoch_rewards_sysvar.parent_blockhash,
            epoch_rewards_sysvar.num_partitions as usize,
        );
        (vote_rewards_accounts, stake_rewards, partition_indices)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::{
                null_tracer,
                partitioned_epoch_rewards::{
                    tests::{
                        build_partitioned_stake_rewards, create_default_reward_bank,
                        create_reward_bank, create_reward_bank_with_specific_stakes,
                        populate_vote_accounts_with_votes, RewardBank, SLOTS_PER_EPOCH,
                    },
                    EpochRewardPhase, EpochRewardStatus, PartitionedStakeRewards,
                    StartBlockHeightAndPartitionedRewards,
                },
                tests::create_genesis_config,
                RewardInfo, VoteReward,
            },
            genesis_utils::{self, deactivate_features, GenesisConfigInfo},
            stake_account::StakeAccount,
            stake_utils,
            stakes::{tests::create_staked_node_accounts, Stakes},
        },
        agave_feature_set::{delay_commission_updates, FeatureSet},
        rayon::ThreadPoolBuilder,
        solana_account::{
            accounts_equal, state_traits::StateMut, AccountSharedData, ReadableAccount,
        },
        solana_accounts_db::partitioned_rewards::PartitionedEpochRewardsConfig,
        solana_epoch_schedule::EpochSchedule,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_reward_info::RewardType,
        solana_signer::Signer,
        solana_stake_interface::{
            stake_flags::StakeFlags,
            state::{Authorized, Delegation, Meta, Stake, StakeStateV2},
        },
        solana_vote_interface::state::{VoteInit, VoteStateV4, VoteStateVersions},
        solana_vote_program::vote_state,
        std::{
            collections::HashSet,
            sync::{Arc, RwLockReadGuard},
        },
        test_case::test_case,
    };

    #[test]
    fn test_store_vote_accounts_partitioned() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let expected_vote_rewards_num = 100;

        let vote_rewards = (0..expected_vote_rewards_num)
            .map(|_| (Pubkey::new_unique(), VoteReward::new_random()))
            .collect::<Vec<_>>();

        let mut vote_rewards_account = VoteRewardsAccounts::default();
        vote_rewards
            .iter()
            .for_each(|(vote_key, vote_reward_info)| {
                let info = RewardInfo {
                    reward_type: RewardType::Voting,
                    lamports: vote_reward_info.vote_rewards as i64,
                    post_balance: vote_reward_info.vote_rewards,
                    commission: Some(vote_reward_info.commission),
                };
                vote_rewards_account.accounts_with_rewards.push((
                    *vote_key,
                    info,
                    vote_reward_info.vote_account.clone(),
                ));
                vote_rewards_account.total_vote_rewards_lamports += vote_reward_info.vote_rewards;
            });

        let metrics = RewardsMetrics::default();

        let total_vote_rewards = vote_rewards_account.total_vote_rewards_lamports;
        bank.store_vote_accounts_partitioned(&vote_rewards_account, &metrics);
        assert_eq!(
            expected_vote_rewards_num,
            vote_rewards_account.accounts_with_rewards.len()
        );
        assert_eq!(
            vote_rewards
                .iter()
                .map(|(_, vote_reward_info)| vote_reward_info.vote_rewards)
                .sum::<u64>(),
            total_vote_rewards
        );

        // load accounts to make sure they were stored correctly
        vote_rewards
            .iter()
            .for_each(|(vote_key, vote_reward_info)| {
                let loaded_account = bank
                    .load_slow_with_fixed_root(&bank.ancestors, vote_key)
                    .unwrap();
                assert!(accounts_equal(
                    &loaded_account.0,
                    &vote_reward_info.vote_account
                ));
            });
    }

    #[test]
    fn test_store_vote_accounts_partitioned_empty() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let expected = 0;
        let vote_rewards = VoteRewardsAccounts::default();
        let metrics = RewardsMetrics::default();
        let total_vote_rewards = vote_rewards.total_vote_rewards_lamports;

        bank.store_vote_accounts_partitioned(&vote_rewards, &metrics);
        assert_eq!(expected, vote_rewards.accounts_with_rewards.len());
        assert_eq!(0, total_vote_rewards);
    }

    #[test]
    /// Test rewards computation and partitioned rewards distribution at the epoch boundary
    fn test_rewards_computation() {
        agave_logger::setup();

        // Delegations with sufficient stake to get rewards (2 SOL).
        let delegations_with_rewards = 100;
        // Delegations with insufficient stake (0.5 SOL).
        let delegations_without_rewards = 10;
        let stakes = (0..delegations_with_rewards)
            .map(|_| 2_000_000_000)
            .chain((0..delegations_without_rewards).map(|_| 500_000_000))
            .collect::<Vec<_>>();
        let bank = create_reward_bank_with_specific_stakes(
            stakes,
            PartitionedEpochRewardsConfig::default().stake_account_stores_per_block,
            SLOTS_PER_EPOCH,
        )
        .0
        .bank;

        // Calculate rewards
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let mut rewards_metrics = RewardsMetrics::default();
        let expected_rewards = 100_000_000_000;

        let stakes = bank.stakes_cache.stakes();
        let rewarded_epoch = 1;
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);
        let calculated_rewards = bank.calculate_validator_rewards(
            &stake_history,
            &stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            expected_rewards,
            null_tracer(),
            &thread_pool,
            &mut rewards_metrics,
        );

        let vote_rewards = &calculated_rewards.as_ref().unwrap().vote_rewards_accounts;
        let stake_rewards = &calculated_rewards
            .as_ref()
            .unwrap()
            .stake_reward_calculation;

        let total_vote_rewards: u64 = vote_rewards
            .accounts_with_rewards
            .iter()
            .map(|(_, reward_info, _)| reward_info.lamports)
            .sum::<i64>() as u64;

        // assert that total rewards matches the sum of vote rewards and stake rewards
        assert_eq!(
            stake_rewards.total_stake_rewards_lamports + total_vote_rewards,
            expected_rewards
        );

        // assert that number of stake rewards matches
        assert_eq!(
            stake_rewards.stake_rewards.num_rewards(),
            delegations_with_rewards
        );
    }

    #[test]
    fn test_rewards_point_calculation() {
        agave_logger::setup();

        let expected_num_delegations = 100;
        let RewardBank { bank, .. } =
            create_default_reward_bank(expected_num_delegations, SLOTS_PER_EPOCH).0;

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let rewards_metrics = RewardsMetrics::default();
        let expected_rewards = 100_000_000_000;

        let stakes: RwLockReadGuard<Stakes<StakeAccount<Delegation>>> = bank.stakes_cache.stakes();
        let rewarded_epoch = bank.epoch().saturating_sub(1);
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);

        let point_value = bank.calculate_reward_points_partitioned(
            &stake_history,
            &stake_delegations,
            &cached_vote_accounts,
            expected_rewards,
            &thread_pool,
            &rewards_metrics,
        );

        assert!(point_value.is_some());
        assert_eq!(point_value.as_ref().unwrap().rewards, expected_rewards);
        assert_eq!(point_value.as_ref().unwrap().points, 8400000000000);
    }

    #[test]
    fn test_rewards_point_calculation_empty() {
        agave_logger::setup();

        // bank with no rewards to distribute
        let (genesis_config, _mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let rewards_metrics: RewardsMetrics = RewardsMetrics::default();
        let expected_rewards = 100_000_000_000;
        let stakes: RwLockReadGuard<Stakes<StakeAccount<Delegation>>> = bank.stakes_cache.stakes();
        let rewarded_epoch = bank.epoch().saturating_sub(1);
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);

        let point_value = bank.calculate_reward_points_partitioned(
            &stake_history,
            &stake_delegations,
            &cached_vote_accounts,
            expected_rewards,
            &thread_pool,
            &rewards_metrics,
        );

        assert!(point_value.is_none());
    }

    struct EpochOperations {
        epoch: Epoch,
        vote_operations: Vec<(Pubkey, VoteOperations)>,
    }

    #[derive(Default)]
    struct VoteOperations {
        expected_reward_commission: Option<u8>,
        // ops to perform before epoch ends
        create_with_balance: Option<u64>,
        delegate_stake_amount: Option<u64>,
        new_commission: Option<u8>,
        earned_credits: Option<u64>,
    }

    fn recalculate_vote_rewards_for_tests(
        bank: &Bank,
        vote_address: &Pubkey,
    ) -> Option<RewardInfo> {
        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let (vote_rewards_accounts, ..) =
            bank.recalculate_stake_rewards(&epoch_rewards_sysvar, &thread_pool);
        vote_rewards_accounts
            .accounts_with_rewards
            .iter()
            .find(|(reward_address, ..)| reward_address == vote_address)
            .map(|(_, reward, _)| *reward)
    }

    fn apply_epoch_operations(mut bank: Bank, op: EpochOperations) -> Bank {
        assert_eq!(bank.epoch(), op.epoch);
        for (vote_address, vote_op) in &op.vote_operations {
            if let Some(balance) = &vote_op.create_with_balance {
                let vote_state = VoteStateVersions::V4(Box::new(VoteStateV4::new_with_defaults(
                    vote_address,
                    &VoteInit {
                        node_pubkey: Pubkey::new_unique(),
                        authorized_voter: Pubkey::new_unique(),
                        authorized_withdrawer: Pubkey::new_unique(),
                        commission: 0,
                    },
                    &bank.clock(),
                )));
                let mut account = solana_account::AccountSharedData::new(
                    *balance,
                    VoteStateV4::size_of(),
                    &solana_vote_program::id(),
                );
                account.serialize_data(&vote_state).unwrap();
                bank.store_account(vote_address, &account);
            }

            if let Some(stake_amount) = &vote_op.delegate_stake_amount {
                let size = StakeStateV2::size_of();
                let rent_exempt_reserve = bank.rent_collector().rent.minimum_balance(size);
                let lamports = rent_exempt_reserve + stake_amount;
                let mut stake_account =
                    AccountSharedData::new(lamports, size, &solana_sdk_ids::stake::id());

                let meta = Meta {
                    authorized: Authorized::auto(&Pubkey::new_unique()),
                    rent_exempt_reserve,
                    ..Meta::default()
                };

                let stake = Stake {
                    delegation: Delegation::new(vote_address, *stake_amount, bank.epoch()),
                    credits_observed: 0,
                };

                stake_account
                    .set_state(&StakeStateV2::Stake(meta, stake, StakeFlags::empty()))
                    .expect("set_state");

                bank.store_account(&Pubkey::new_unique(), &stake_account);
            }

            let modify_vote_state = |modify_fn: &dyn Fn(&mut VoteStateV4)| {
                let mut vote_account = bank.get_account(vote_address).unwrap();
                let vote_state_versions = vote_account
                    .deserialize_data::<VoteStateVersions>()
                    .unwrap();
                let VoteStateVersions::V4(mut vote_state) = vote_state_versions else {
                    panic!("unexpected version");
                };

                modify_fn(&mut vote_state);

                vote_account
                    .serialize_data(&VoteStateVersions::V4(vote_state))
                    .unwrap();
                bank.store_account(vote_address, &vote_account);
            };

            if let Some(commission) = vote_op.new_commission {
                modify_vote_state(&|vote_state: &mut VoteStateV4| {
                    vote_state.inflation_rewards_commission_bps = commission as u16 * 100;
                });
            }

            if let Some(earned_credits) = vote_op.earned_credits {
                modify_vote_state(&|vote_state: &mut VoteStateV4| {
                    let last_credits = vote_state
                        .epoch_credits
                        .last()
                        .map(|(_epoch, credits, _)| *credits)
                        .unwrap_or(0);
                    vote_state.epoch_credits.push((
                        bank.epoch,
                        last_credits + earned_credits,
                        last_credits,
                    ));
                });
            }
        }

        // Advance bank to next epoch
        let slot = bank.slot + SLOTS_PER_EPOCH;
        let prev_bank = Arc::new(bank);
        bank = Bank::new_from_parent(prev_bank.clone(), &Pubkey::new_unique(), slot);

        for (vote_address, vote_op) in &op.vote_operations {
            let expected_commission = vote_op.expected_reward_commission;
            let recalculated_vote_reward = recalculate_vote_rewards_for_tests(&bank, vote_address);
            let vote_reward = bank
                .rewards
                .read()
                .unwrap()
                .iter()
                .find(|(address, _reward)| address == vote_address)
                .map(|(_address, reward)| *reward);

            let prev_vote_balance = prev_bank
                .get_account(vote_address)
                .unwrap_or_default()
                .lamports();
            let vote_balance = bank.get_balance(vote_address);

            if let Some(expected_commission) = &expected_commission {
                let reward_lamports = vote_balance - prev_vote_balance;
                let expected_vote_reward = RewardInfo {
                    commission: Some(*expected_commission),
                    reward_type: RewardType::Voting,
                    lamports: reward_lamports as i64,
                    post_balance: vote_balance,
                };

                assert_eq!(
                    vote_reward,
                    Some(expected_vote_reward),
                    "epoch {}: unexpected reward info",
                    op.epoch
                );

                assert_eq!(
                    recalculated_vote_reward,
                    Some(expected_vote_reward),
                    "epoch {}: unexpected recalculated reward info",
                    op.epoch
                );
            } else {
                assert!(
                    vote_reward.map(|reward| reward.lamports).unwrap_or(0) == 0,
                    "epoch {}: expected no reward",
                    op.epoch
                );
                assert!(
                    recalculated_vote_reward
                        .map(|reward| reward.lamports)
                        .unwrap_or(0)
                        == 0,
                    "epoch {}: expected no recalculated reward",
                    op.epoch
                );
            }
        }

        bank
    }

    #[test_case(true; "delay_commission_updates")]
    #[test_case(false; "instant_commission_updates")]
    fn test_calculate_stake_vote_rewards_new_vote_account(delay_commission_updates: bool) {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);
        if !delay_commission_updates {
            deactivate_features(&mut genesis_config, &vec![delay_commission_updates::id()]);
        }

        let mut bank = Bank::new_for_tests(&genesis_config);
        let vote_address = Pubkey::new_unique();

        // No reward should be given in the epoch that a vote account is
        // delegated to for the first time
        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        create_with_balance: Some(LAMPORTS_PER_SOL),
                        new_commission: Some(1),
                        earned_credits: Some(1000),
                        delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // Check that if a vote account didn't exist two epochs ago (normal for
        // new vote accounts), that the reward commission falls back to the
        // commission from the end of the rewarded epoch.
        bank = {
            let expected_commission = if delay_commission_updates { 1 } else { 2 };
            apply_epoch_operations(
                bank,
                EpochOperations {
                    epoch: 1,
                    vote_operations: vec![(
                        vote_address,
                        VoteOperations {
                            new_commission: Some(2),
                            earned_credits: Some(1000),
                            expected_reward_commission: Some(expected_commission),
                            ..VoteOperations::default()
                        },
                    )],
                },
            )
        };

        let expected_commission = if delay_commission_updates { 1 } else { 3 };
        apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 2,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(3),
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(expected_commission),
                        ..VoteOperations::default()
                    },
                )],
            },
        );
    }

    #[test]
    fn test_calculate_stake_vote_rewards_prestaked_vote_account() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);
        let mut bank = Bank::new_for_tests(&genesis_config);
        assert!(bank.feature_set.is_active(&delay_commission_updates::id()));

        let vote_address = Pubkey::new_unique();
        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // Check that if a new vote account is somehow already staked and
        // earning rewards in the epoch in which it was created, the reward
        // commission falls back to the latest commission rate for that epoch
        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        create_with_balance: Some(LAMPORTS_PER_SOL),
                        new_commission: Some(1),
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(1),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // And similarly, check that if a vote account didn't exist two epochs
        // ago, the reward commission falls back to the commission from the
        // previous epoch
        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 2,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(2),
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(1),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 3,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(3),
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(1),
                        ..VoteOperations::default()
                    },
                )],
            },
        );
    }

    #[test]
    fn test_calculate_stake_vote_rewards_genesis_vote_account() {
        let GenesisConfigInfo {
            mut genesis_config,
            voting_keypair,
            ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);
        let mut bank = Bank::new_for_tests(&genesis_config);
        assert!(bank.feature_set.is_active(&delay_commission_updates::id()));

        let genesis_vote_address = voting_keypair.pubkey();

        // Check that staked genesis vote accounts use the initial commission
        // rate for the first reward epoch
        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        new_commission: Some(1),
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(0),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // Check that staked genesis vote accounts use the initial commission
        // rate for the second reward epoch too.
        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        new_commission: Some(2),
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(0),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 2,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(1),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        bank = apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 3,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(2),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        apply_epoch_operations(
            bank,
            EpochOperations {
                epoch: 4,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expected_reward_commission: Some(2),
                        ..VoteOperations::default()
                    },
                )],
            },
        );
    }

    #[test]
    fn test_calculate_stake_vote_rewards() {
        agave_logger::setup();

        let expected_num_delegations = 1;
        let RewardBank {
            bank,
            voters,
            stakers,
        } = create_default_reward_bank(expected_num_delegations, SLOTS_PER_EPOCH).0;

        let vote_pubkey = voters.first().unwrap();
        let stake_pubkey = *stakers.first().unwrap();
        let stake_account = bank
            .load_slow_with_fixed_root(&bank.ancestors, &stake_pubkey)
            .unwrap()
            .0;

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let mut rewards_metrics = RewardsMetrics::default();

        let point_value = PointValue {
            rewards: 100000, // lamports to split
            points: 1000,    // over these points
        };
        let tracer = |_event: &RewardCalculationEvent| {};
        let reward_calc_tracer = Some(tracer);
        let rewarded_epoch = bank.epoch();
        let stakes: RwLockReadGuard<Stakes<StakeAccount<Delegation>>> = bank.stakes_cache.stakes();
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);
        let (vote_rewards_accounts, stake_reward_calculation) = bank.calculate_stake_vote_rewards(
            &stake_history,
            &stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            point_value,
            &thread_pool,
            reward_calc_tracer,
            &mut rewards_metrics,
        );
        drop(stakes);

        let vote_account = bank
            .load_slow_with_fixed_root(&bank.ancestors, vote_pubkey)
            .unwrap()
            .0;
        let vote_state = VoteStateV4::deserialize(vote_account.data(), vote_pubkey).unwrap();

        assert_eq!(vote_rewards_accounts.accounts_with_rewards.len(), 1);
        let (vote_pubkey_from_result, rewards, account) =
            &vote_rewards_accounts.accounts_with_rewards[0];
        let vote_rewards = 0;
        let commission = (vote_state.inflation_rewards_commission_bps / 100) as u8;
        assert_eq!(account.lamports(), vote_account.lamports());
        assert!(accounts_equal(account, &vote_account));
        assert_eq!(
            *rewards,
            RewardInfo {
                reward_type: RewardType::Voting,
                lamports: vote_rewards as i64,
                post_balance: vote_account.lamports(),
                commission: Some(commission),
            }
        );
        assert_eq!(vote_pubkey_from_result, vote_pubkey);

        assert_eq!(stake_reward_calculation.stake_rewards.num_rewards(), 1);
        let expected_reward = {
            let stake_reward = 8_400_000_000_000;
            let stake_state: StakeStateV2 = stake_account.state().unwrap();
            let mut stake = stake_state.stake().unwrap();
            stake.credits_observed = vote_state.credits();
            stake.delegation.stake += stake_reward;
            PartitionedStakeReward {
                stake,
                stake_pubkey,
                stake_reward,
                commission,
            }
        };
        assert_eq!(
            stake_reward_calculation
                .stake_rewards
                .get(0)
                .unwrap()
                .as_ref()
                .unwrap(),
            &expected_reward
        );
    }

    fn compare_stake_rewards(
        expected_stake_rewards: &[PartitionedStakeRewards],
        received_stake_rewards: &[PartitionedStakeRewards],
    ) {
        for (i, partition) in received_stake_rewards.iter().enumerate() {
            let expected_partition = &expected_stake_rewards[i];
            assert_eq!(partition, expected_partition);
        }
    }

    #[test]
    fn test_recalculate_stake_rewards() {
        let expected_num_delegations = 4;
        let num_rewards_per_block = 2;
        // Distribute 4 rewards over 2 blocks
        let (RewardBank { bank, .. }, _) = create_reward_bank(
            expected_num_delegations,
            num_rewards_per_block,
            SLOTS_PER_EPOCH,
        );
        let rewarded_epoch = bank.epoch();

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let mut rewards_metrics = RewardsMetrics::default();
        let stakes = bank.stakes_cache.stakes();
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);
        let PartitionedRewardsCalculation {
            stake_rewards:
                StakeRewardCalculation {
                    stake_rewards: expected_stake_rewards,
                    ..
                },
            ..
        } = bank.calculate_rewards_for_partitioning(
            &stake_history,
            &stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            null_tracer(),
            &thread_pool,
            &mut rewards_metrics,
        );
        drop(stakes);

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let (_, recalculated_rewards, recalculated_partition_indices) =
            bank.recalculate_stake_rewards(&epoch_rewards_sysvar, &thread_pool);

        let recalculated_rewards =
            build_partitioned_stake_rewards(&recalculated_rewards, &recalculated_partition_indices);

        let expected_partition_indices = hash_rewards_into_partitions(
            &expected_stake_rewards,
            &epoch_rewards_sysvar.parent_blockhash,
            epoch_rewards_sysvar.num_partitions as usize,
        );

        let expected_stake_rewards_partitioned =
            build_partitioned_stake_rewards(&expected_stake_rewards, &expected_partition_indices);

        assert_eq!(
            expected_stake_rewards_partitioned.len(),
            recalculated_rewards.len()
        );
        compare_stake_rewards(&expected_stake_rewards_partitioned, &recalculated_rewards);

        // Advance to first distribution block, ie. child block of the epoch
        // boundary; slot is advanced 2 to demonstrate that distribution works
        // on block-height, not slot
        let new_slot = bank.slot() + 2;
        let bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), new_slot));

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let (_, recalculated_rewards, recalculated_partition_indices) =
            bank.recalculate_stake_rewards(&epoch_rewards_sysvar, &thread_pool);

        // Note that recalculated rewards are **NOT** the same as expected
        // rewards, which were calculated before any distribution. This is
        // because "Recalculated rewards" doesn't include already distributed
        // stake rewards. Therefore, the partition_indices are different too.
        // However, the actual rewards for the remaining partitions should be
        // the same. The following code use the test helper function to build
        // the partitioned stake rewards for the remaining partitions and verify
        // that they are the same.
        let recalculated_rewards =
            build_partitioned_stake_rewards(&recalculated_rewards, &recalculated_partition_indices);
        assert_eq!(
            expected_stake_rewards_partitioned.len(),
            recalculated_rewards.len()
        );
        // First partition has already been distributed, so recalculation
        // returns 0 rewards
        assert_eq!(recalculated_rewards[0].num_rewards(), 0);
        let starting_index = (bank.block_height() + 1
            - epoch_rewards_sysvar.distribution_starting_block_height)
            as usize;
        compare_stake_rewards(
            &expected_stake_rewards_partitioned[starting_index..],
            &recalculated_rewards[starting_index..],
        );

        // Advance to last distribution slot
        let new_slot = bank.slot() + 1;
        let bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), new_slot));

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        assert!(!epoch_rewards_sysvar.active);
        // Recalculation would panic, tested separately
    }

    #[test]
    #[should_panic]
    fn test_recalculate_stake_rewards_distribution_complete() {
        let expected_num_delegations = 2;
        let num_rewards_per_block = 2;
        // Distribute 2 rewards over 1 block
        let (RewardBank { bank, .. }, _) = create_reward_bank(
            expected_num_delegations,
            num_rewards_per_block,
            SLOTS_PER_EPOCH,
        );
        let rewarded_epoch = bank.epoch();

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let mut rewards_metrics = RewardsMetrics::default();
        let stakes = bank.stakes_cache.stakes();
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);
        let PartitionedRewardsCalculation {
            stake_rewards:
                StakeRewardCalculation {
                    stake_rewards: expected_stake_rewards,
                    ..
                },
            ..
        } = bank.calculate_rewards_for_partitioning(
            &stake_history,
            &stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            null_tracer(),
            &thread_pool,
            &mut rewards_metrics,
        );
        drop(stakes);

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let expected_partition_indices = hash_rewards_into_partitions(
            &expected_stake_rewards,
            &epoch_rewards_sysvar.parent_blockhash,
            epoch_rewards_sysvar.num_partitions as usize,
        );
        let expected_stake_rewards =
            build_partitioned_stake_rewards(&expected_stake_rewards, &expected_partition_indices);

        let (_, recalculated_rewards, recalculated_partition_indices) =
            bank.recalculate_stake_rewards(&epoch_rewards_sysvar, &thread_pool);
        let recalculated_rewards =
            build_partitioned_stake_rewards(&recalculated_rewards, &recalculated_partition_indices);

        assert_eq!(expected_stake_rewards.len(), recalculated_rewards.len());
        compare_stake_rewards(&expected_stake_rewards, &recalculated_rewards);

        // Advance to first distribution slot
        let new_slot = bank.slot() + 1;
        let bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), new_slot));

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        assert!(!epoch_rewards_sysvar.active);
        // Should panic
        let _recalculated_rewards =
            bank.recalculate_stake_rewards(&epoch_rewards_sysvar, &thread_pool);
    }

    #[test]
    fn test_recalculate_partitioned_rewards() {
        let expected_num_delegations = 3;
        let num_rewards_per_block = 2;
        // Distribute 4 rewards over 2 blocks
        let mut stakes = vec![2_000_000_000; expected_num_delegations];
        // Add stake large enough to be affected by total-rewards discrepancy
        stakes.push(40_000_000_000);
        let (RewardBank { bank, .. }, _) = create_reward_bank_with_specific_stakes(
            stakes,
            num_rewards_per_block,
            SLOTS_PER_EPOCH - 1,
        );
        let rewarded_epoch = bank.epoch();

        // Advance to next epoch boundary to update EpochStakes Kludgy because
        // mutable Bank methods require the bank not be Arc-wrapped.
        let new_slot = bank.slot() + 1;
        let mut bank = Bank::new_from_parent(bank, &Pubkey::default(), new_slot);
        let expected_starting_block_height = bank.block_height() + 1;

        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let mut rewards_metrics = RewardsMetrics::default();
        let stakes = bank.stakes_cache.stakes();
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);
        let PartitionedRewardsCalculation {
            stake_rewards:
                StakeRewardCalculation {
                    stake_rewards: expected_stake_rewards,
                    ..
                },
            point_value,
            ..
        } = bank.calculate_rewards_for_partitioning(
            &stake_history,
            &stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            null_tracer(),
            &thread_pool,
            &mut rewards_metrics,
        );
        drop(stakes);

        bank.recalculate_partitioned_rewards_if_active(|| &thread_pool);
        let EpochRewardStatus::Active(EpochRewardPhase::Distribution(
            StartBlockHeightAndPartitionedRewards {
                distribution_starting_block_height,
                all_stake_rewards: ref recalculated_rewards,
                ref partition_indices,
            },
        )) = bank.epoch_reward_status
        else {
            panic!("{:?} not active", bank.epoch_reward_status);
        };
        assert_eq!(
            expected_starting_block_height,
            distribution_starting_block_height
        );

        let recalculated_rewards =
            build_partitioned_stake_rewards(recalculated_rewards, partition_indices);

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let expected_partition_indices = hash_rewards_into_partitions(
            &expected_stake_rewards,
            &epoch_rewards_sysvar.parent_blockhash,
            epoch_rewards_sysvar.num_partitions as usize,
        );
        let expected_stake_rewards =
            build_partitioned_stake_rewards(&expected_stake_rewards, &expected_partition_indices);

        assert_eq!(expected_stake_rewards.len(), recalculated_rewards.len());
        compare_stake_rewards(&expected_stake_rewards, &recalculated_rewards);

        let sysvar = bank.get_epoch_rewards_sysvar();
        assert_eq!(point_value.rewards, sysvar.total_rewards);

        // Advance to first distribution slot
        let mut bank =
            Bank::new_from_parent(Arc::new(bank), &Pubkey::default(), SLOTS_PER_EPOCH + 1);

        bank.recalculate_partitioned_rewards_if_active(|| &thread_pool);
        let EpochRewardStatus::Active(EpochRewardPhase::Distribution(
            StartBlockHeightAndPartitionedRewards {
                distribution_starting_block_height,
                all_stake_rewards: ref recalculated_rewards,
                ref partition_indices,
            },
        )) = bank.epoch_reward_status
        else {
            panic!("{:?} not active", bank.epoch_reward_status);
        };

        // Note that recalculated rewards are **NOT** the same as expected
        // rewards, which were calculated before any distribution. This is
        // because "Recalculated rewards" doesn't include already distributed
        // stake rewards. Therefore, the partition_indices are different too.
        // However, the actual rewards for the remaining partitions should be
        // the same. The following code use the test helper function to build
        // the partitioned stake rewards for the remaining partitions and verify
        // that they are the same.
        let recalculated_rewards =
            build_partitioned_stake_rewards(recalculated_rewards, partition_indices);
        assert_eq!(
            expected_starting_block_height,
            distribution_starting_block_height
        );
        assert_eq!(expected_stake_rewards.len(), recalculated_rewards.len());
        // First partition has already been distributed, so recalculation
        // returns 0 rewards
        assert_eq!(recalculated_rewards[0].num_rewards(), 0);
        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let starting_index = (bank.block_height() + 1
            - epoch_rewards_sysvar.distribution_starting_block_height)
            as usize;
        compare_stake_rewards(
            &expected_stake_rewards[starting_index..],
            &recalculated_rewards[starting_index..],
        );

        // Advance to last distribution slot
        let mut bank =
            Bank::new_from_parent(Arc::new(bank), &Pubkey::default(), SLOTS_PER_EPOCH + 2);
        bank.recalculate_partitioned_rewards_if_active(|| &thread_pool);
        assert_eq!(bank.epoch_reward_status, EpochRewardStatus::Inactive);
    }

    #[test]
    fn test_initialize_after_snapshot_restore() {
        let expected_num_stake_rewards = 3;
        let num_rewards_per_block = 2;
        // Distribute 4 rewards over 2 blocks
        let stakes = vec![
            100_000_000,   // under min delegation
            2_000_000_000, // valid delegation
            3_000_000_000, // valid delegation
            4_000_000_000, // valid delegation
        ];
        let (RewardBank { bank, .. }, _) = create_reward_bank_with_specific_stakes(
            stakes,
            num_rewards_per_block,
            SLOTS_PER_EPOCH - 1,
        );

        // Advance to next epoch boundary
        let new_slot = bank.slot() + 1;
        let mut bank = Bank::new_from_parent(bank, &Pubkey::default(), new_slot);

        let EpochRewardStatus::Active(EpochRewardPhase::Calculation(calculation_status)) =
            bank.epoch_reward_status.clone()
        else {
            panic!("{:?} not active calculation", bank.epoch_reward_status);
        };

        // Reset feature set to default, to simulate snapshot restore
        bank.feature_set = Arc::new(FeatureSet::default());

        // Run post snapshot restore initialization which should first apply
        // active features and then recalculate rewards
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        bank.initialize_after_snapshot_restore(|| &thread_pool);

        let EpochRewardStatus::Active(EpochRewardPhase::Distribution(distribution_status)) =
            bank.epoch_reward_status.clone()
        else {
            panic!("{:?} not active distribution", bank.epoch_reward_status);
        };

        assert_eq!(
            calculation_status.all_stake_rewards,
            distribution_status.all_stake_rewards
        );
        assert_eq!(
            calculation_status.distribution_starting_block_height,
            distribution_status.distribution_starting_block_height
        );
        assert_eq!(
            calculation_status.all_stake_rewards.num_rewards(),
            expected_num_stake_rewards
        );
    }

    #[test]
    fn test_reward_accumulator() {
        let mut accumulator1 = RewardsAccumulator::default();
        let mut accumulator2 = RewardsAccumulator::default();

        let vote_pubkey_a = Pubkey::new_unique();
        let node_pubkey_a = Pubkey::new_unique();
        let vote_account_a = vote_state::create_v4_account_with_authorized(
            &node_pubkey_a,
            &vote_pubkey_a,
            &vote_pubkey_a,
            None,
            2000,
            100,
        );
        let vote_pubkey_b = Pubkey::new_unique();
        let node_pubkey_b = Pubkey::new_unique();
        let vote_account_b = vote_state::create_v4_account_with_authorized(
            &node_pubkey_b,
            &vote_pubkey_b,
            &vote_pubkey_b,
            None,
            2000,
            100,
        );
        let vote_pubkey_c = Pubkey::new_unique();
        let node_pubkey_c = Pubkey::new_unique();
        let vote_account_c = vote_state::create_v4_account_with_authorized(
            &node_pubkey_c,
            &vote_pubkey_c,
            &vote_pubkey_c,
            None,
            2000,
            100,
        );

        accumulator1.add_reward(
            vote_pubkey_a,
            VoteReward {
                vote_account: vote_account_a.clone(),
                commission: 10,
                vote_rewards: 50,
            },
            50,
        );
        accumulator1.add_reward(
            vote_pubkey_b,
            VoteReward {
                vote_account: vote_account_b.clone(),
                commission: 10,
                vote_rewards: 50,
            },
            50,
        );
        accumulator2.add_reward(
            vote_pubkey_b,
            VoteReward {
                vote_account: vote_account_b,
                commission: 10,
                vote_rewards: 30,
            },
            30,
        );
        accumulator2.add_reward(
            vote_pubkey_c,
            VoteReward {
                vote_account: vote_account_c,
                commission: 10,
                vote_rewards: 50,
            },
            50,
        );

        assert_eq!(accumulator1.num_stake_rewards, 2);
        assert_eq!(accumulator1.total_stake_rewards_lamports, 100);
        let vote_reward_a_1 = accumulator1.vote_rewards.get(&vote_pubkey_a).unwrap();
        assert_eq!(vote_reward_a_1.commission, 10);
        assert_eq!(vote_reward_a_1.vote_rewards, 50);
        let vote_reward_b_1 = accumulator1.vote_rewards.get(&vote_pubkey_b).unwrap();
        assert_eq!(vote_reward_b_1.commission, 10);
        assert_eq!(vote_reward_b_1.vote_rewards, 50);

        let vote_reward_b_2 = accumulator2.vote_rewards.get(&vote_pubkey_b).unwrap();
        assert_eq!(vote_reward_b_2.commission, 10);
        assert_eq!(vote_reward_b_2.vote_rewards, 30);
        let vote_reward_c_2 = accumulator2.vote_rewards.get(&vote_pubkey_c).unwrap();
        assert_eq!(vote_reward_c_2.commission, 10);
        assert_eq!(vote_reward_c_2.vote_rewards, 50);

        let accumulator = accumulator1.accumulate_into_larger(accumulator2);

        assert_eq!(accumulator.num_stake_rewards, 4);
        assert_eq!(accumulator.total_stake_rewards_lamports, 180);
        let vote_reward_a = accumulator.vote_rewards.get(&vote_pubkey_a).unwrap();
        assert_eq!(vote_reward_a.commission, 10);
        assert_eq!(vote_reward_a.vote_rewards, 50);
        let vote_reward_b = accumulator.vote_rewards.get(&vote_pubkey_b).unwrap();
        assert_eq!(vote_reward_b.commission, 10);
        // sum of the vote rewards from both accumulators
        assert_eq!(vote_reward_b.vote_rewards, 80);
        let vote_reward_c = accumulator.vote_rewards.get(&vote_pubkey_c).unwrap();
        assert_eq!(vote_reward_c.commission, 10);
        assert_eq!(vote_reward_c.vote_rewards, 50);
    }

    #[test]
    fn test_epoch_rewards_cache_multiple_forks() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);

        const NUM_STAKES: usize = 1000;

        for _i in 0..NUM_STAKES {
            let vote_pubkey = Pubkey::new_unique();
            let stake_pubkey = Pubkey::new_unique();

            genesis_config.accounts.insert(
                vote_pubkey,
                vote_state::create_v4_account_with_authorized(
                    &vote_pubkey,
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    None,
                    0,
                    100_000_000_000,
                )
                .into(),
            );

            let stake_lamports = 1_000_000_000_000;
            let stake_account = stake_utils::create_stake_account(
                &stake_pubkey,
                &vote_pubkey,
                &vote_state::create_v4_account_with_authorized(
                    &vote_pubkey,
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    None,
                    0,
                    100_000_000_000,
                ),
                &genesis_config.rent,
                stake_lamports,
            );
            genesis_config
                .accounts
                .insert(stake_pubkey, stake_account.into());
        }

        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let next_epoch_slot = bank.get_slots_in_epoch(bank.epoch);
        {
            let cache = bank.epoch_rewards_calculation_cache.lock().unwrap();
            assert!(
                !cache.contains_key(&bank.parent_hash()),
                "cache should be empty"
            );
        }

        let bank_fork1 =
            Bank::new_from_parent(Arc::clone(&bank), &Pubkey::default(), next_epoch_slot);
        {
            let cache = bank_fork1.epoch_rewards_calculation_cache.lock().unwrap();
            assert!(
                cache.contains_key(&bank_fork1.parent_hash()),
                "cache should be populated"
            );
        }

        let bank_fork2 = Bank::new_from_parent(bank, &Pubkey::default(), next_epoch_slot);
        {
            let cache = bank_fork2.epoch_rewards_calculation_cache.lock().unwrap();
            assert!(
                cache.contains_key(&bank_fork2.parent_hash()),
                "cache should be populated"
            );
        }
    }

    fn add_voters_and_populate(
        bank: &Arc<Bank>,
        voters: &mut HashSet<Pubkey>,
        stakers: &mut HashSet<Pubkey>,
        count: usize,
        stake_lamports: u64,
        commission: u8,
    ) {
        for _ in 0..count {
            let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
                create_staked_node_accounts(stake_lamports);
            bank.store_account_and_update_capitalization(&vote_pubkey, &vote_account);
            bank.store_account_and_update_capitalization(&stake_pubkey, &stake_account);
            voters.insert(vote_pubkey);
            stakers.insert(stake_pubkey);
        }
        populate_vote_accounts_with_votes(bank, voters.iter().copied(), commission);
    }

    #[allow(clippy::too_many_arguments)]
    fn assert_cached_rewards(
        bank: &Arc<Bank>,
        expected_cache_len: usize,
        expected_voters: &HashSet<Pubkey>,
        expected_stakers: &HashSet<Pubkey>,
        expected_vote_rewards: u64,
        expected_stake_rewards: u64,
        expected_rewards: u64,
        expected_points: u128,
        parent_capitalization: Option<u64>,
    ) {
        let cache = bank.epoch_rewards_calculation_cache.lock().unwrap();
        assert_eq!(cache.len(), expected_cache_len);
        let partitioned = cache.get(&bank.parent_hash()).unwrap().as_ref();
        let VoteRewardsAccounts {
            accounts_with_rewards,
            total_vote_rewards_lamports,
            ..
        } = &partitioned.vote_account_rewards;
        let StakeRewardCalculation {
            stake_rewards,
            total_stake_rewards_lamports,
            ..
        } = &partitioned.stake_rewards;
        let point_value = &partitioned.point_value;
        let voters: HashSet<_> = accounts_with_rewards
            .iter()
            .map(|(pubkey, _reward, _acc)| *pubkey)
            .collect();
        let stakers: HashSet<_> = stake_rewards
            .rewards
            .iter()
            .filter_map(|reward| reward.as_ref())
            .map(|reward| reward.stake_pubkey)
            .collect();
        assert_eq!(expected_voters, &voters);
        assert_eq!(expected_stakers, &stakers);
        assert_eq!(*total_vote_rewards_lamports, expected_vote_rewards);
        assert_eq!(*total_stake_rewards_lamports, expected_stake_rewards);
        assert_eq!(point_value.rewards, expected_rewards);
        assert_eq!(point_value.points, expected_points);
        if let Some(parent_cap) = parent_capitalization {
            assert_eq!(bank.capitalization(), parent_cap + expected_vote_rewards);
        }
    }

    #[test]
    fn test_epoch_boundary() {
        let delegations = 100;
        let stake_lamports = 2_000_000_000;
        let stakes: Vec<_> = (0..delegations).map(|_| stake_lamports).collect();
        let (
            RewardBank {
                bank: bank1,
                voters,
                stakers,
                ..
            },
            _bank_forks,
        ) = create_reward_bank_with_specific_stakes(
            stakes,
            PartitionedEpochRewardsConfig::default().stake_account_stores_per_block,
            SLOTS_PER_EPOCH,
        );
        let mut voters: HashSet<_> = voters.into_iter().collect();
        let mut stakers: HashSet<_> = stakers.into_iter().collect();

        // The sysvar account holds the rent-exempt lamport added after
        // reward calculation, so the bank capitalization exceeds the cached
        // value by this amount.
        let epoch_rewards_sysvar_balance = bank1.get_balance(&solana_sysvar::epoch_rewards::id());
        assert_eq!(epoch_rewards_sysvar_balance, 1);

        assert_cached_rewards(
            &bank1,
            1,                     // expected_cache_len
            &voters,               // expected_voters
            &stakers,              // expected_stakers
            0,                     // expected_vote_rewards
            12300,                 // expected_stake_rewards
            12392,                 // expected_rewards
            8_400_000_000_000u128, // expected_points
            None,                  // parent_capitalization
        );

        add_voters_and_populate(&bank1, &mut voters, &mut stakers, 5, 5_000_000_000, 10);
        let parent_capitalization = bank1.capitalization();

        let bank2 = Arc::new(Bank::new_from_parent(
            Arc::clone(&bank1),
            &Pubkey::default(),
            SLOTS_PER_EPOCH * 2,
        ));

        assert_cached_rewards(
            &bank2,
            2,                           // expected_cache_len
            &voters,                     // expected_voters
            &stakers,                    // expected_stakers
            145,                         // expected_vote_rewards
            13010,                       // expected_stake_rewards
            13163,                       // expected_rewards
            9_450_000_000_000u128,       // expected_points
            Some(parent_capitalization), // parent_capitalization
        );

        add_voters_and_populate(&bank2, &mut voters, &mut stakers, 10, 8_000_000_000, 10);
        let parent_capitalization = bank2.capitalization();

        let bank3 = Arc::new(Bank::new_from_parent(
            Arc::clone(&bank2),
            &Pubkey::default(),
            SLOTS_PER_EPOCH * 3,
        ));

        assert_cached_rewards(
            &bank3,
            3,                           // expected_cache_len
            &voters,                     // expected_voters
            &stakers,                    // expected_stakers
            525,                         // expected_vote_rewards
            15030,                       // expected_stake_rewards
            15629,                       // expected_rewards
            12_810_000_000_000u128,      // expected_points
            Some(parent_capitalization), // parent_capitalization
        );
    }
}
