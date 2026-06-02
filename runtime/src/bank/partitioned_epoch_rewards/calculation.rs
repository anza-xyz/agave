use {
    super::{
        Bank, CachedVoteAccounts, CalculateValidatorRewardsResult, EpochRewardCalculateParamInfo,
        PartitionedRewardsCalculation, PartitionedStakeReward, PartitionedStakeRewards,
        REWARD_CALCULATION_NUM_BLOCKS, RewardCommission, RewardCommissionAccounts,
        RewardCommissionAccountsStorable, RewardCommissionLamportAmounts, RewardCommissions,
        StakeRewardCalculation, epoch_rewards_hasher::hash_rewards_into_partitions,
    },
    crate::{
        alpenglow_epoch_type::AlpenglowEpochType,
        bank::{
            RewardCalcTracer, RewardCalculationEvent, RewardsMetrics,
            fee_distribution::ExternalCollectorType, null_tracer,
        },
        inflation_rewards::{
            adjust_delegation_for_rent,
            points::{
                CalculationEnvironment, DelegatedVoteState, PointValue, calculate_points_for_tower,
            },
            redeem_rewards,
        },
        reward_info::RewardInfo,
        stake_account::StakeAccount,
        stakes::Stakes,
    },
    log::{debug, info},
    rayon::{
        ThreadPool,
        iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_account::{ReadableAccount, WritableAccount},
    solana_clock::{Epoch, Slot},
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_reward_info::RewardType,
    solana_stake_interface::{stake_history::StakeHistory, state::Delegation},
    solana_sysvar::epoch_rewards::EpochRewards,
    std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

#[derive(Debug)]
struct DelegationRewards {
    stake_reward: PartitionedStakeReward,
    commission_pubkey: Pubkey,
    reward_commission: RewardCommission,
}

#[derive(Default)]
struct RewardsAccumulator {
    reward_commissions: RewardCommissions,
    num_stake_rewards: usize,
    total_stake_rewards_lamports: u64,
}

/// Merge the lamport and `is_vote_account` fields of two `RewardCommission`s
///
/// This pays special attention to the case where `is_vote_account` does not
/// match, which can happen in the following situation:
///
/// * a vote account A sets the inflation collector to valid system account B
/// * at some point in the future, that system account B gets allocated and
///   initialized as a vote account B
/// * vote account B sets itself as the inflation reward collector
///
/// In that situation, the rewards for vote account A will get burned, but the
/// rewards for vote account B will not. According to the rules of SIMD-0232,
/// a collector account must either be the vote account itself or a system
/// account that fulfills certain criteria. In the case of vote account A, we
/// are already sure that the collector account is invalid.
///
/// NOTE: if vote account B sets a system account as its inflation collector,
/// then the commission lamports for vote account A will NOT get burned here,
/// but will get burned during `load_and_reward_commission_accounts`
fn accumulate_lamports(src: &RewardCommission, dst: &mut RewardCommission) {
    match (src.is_vote_account, dst.is_vote_account) {
        (false, true) => {
            // Don't accumulate, burn everything in the source
            // reward commission entry.
            //
            // NOTE: There shouldn't be any burned lamports in the
            // source entry, but we're defensive
            dst.burned_lamports = dst
                .burned_lamports
                .saturating_add(src.commission_lamports)
                .saturating_add(src.burned_lamports);
        }
        (true, false) => {
            // The commission lamports on the source are the only
            // ones that get distributed, all others get burned.
            //
            // NOTE: There shouldn't be any burned lamports in the
            // destination entry, but we're defensive
            dst.is_vote_account = true;
            dst.burned_lamports = dst
                .burned_lamports
                .saturating_add(dst.commission_lamports)
                .saturating_add(src.burned_lamports);
            dst.commission_lamports = src.commission_lamports;
        }
        _ => {
            // Normal case, just accumulate both
            dst.commission_lamports = dst
                .commission_lamports
                .saturating_add(src.commission_lamports);
            dst.burned_lamports = dst.burned_lamports.saturating_add(src.burned_lamports);
        }
    }
}

impl RewardsAccumulator {
    fn add_reward(
        &mut self,
        commission_pubkey: Pubkey,
        reward_commission: RewardCommission,
        stakers_reward: u64,
    ) {
        self.reward_commissions
            .entry(commission_pubkey)
            .and_modify(|dst_reward_commission| {
                accumulate_lamports(&reward_commission, dst_reward_commission);
            })
            .or_insert(reward_commission);
        self.num_stake_rewards = self.num_stake_rewards.saturating_add(1);
        self.total_stake_rewards_lamports = self
            .total_stake_rewards_lamports
            .saturating_add(stakers_reward);
    }

    /// Merges two instances by combining their reward commissions and stake rewards.
    ///
    /// To minimize reallocations, the instance with more reward commissions is used
    /// as the base and the smaller instance is merged into it.
    fn accumulate_into_larger(self, rhs: Self) -> Self {
        // Check which instance has more reward commissions. Treat the bigger one
        // as a destination, which is going to be extended. This way we make
        // the reallocation as small as possible.
        let (mut dst, src) = if self.reward_commissions.len() >= rhs.reward_commissions.len() {
            (self, rhs)
        } else {
            (rhs, self)
        };
        for (commission_pubkey, reward_commission) in src.reward_commissions {
            dst.reward_commissions
                .entry(commission_pubkey)
                .and_modify(|dst_reward_commission: &mut RewardCommission| {
                    accumulate_lamports(&reward_commission, dst_reward_commission);
                })
                .or_insert(reward_commission);
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
    ///
    /// Returns the distributed epoch validator rewards, not including lamports
    /// distributed to the incinerator.
    pub(in crate::bank) fn begin_partitioned_rewards(
        &mut self,
        parent_epoch: Epoch,
        parent_slot: Slot,
        parent_block_height: u64,
        rewards_calculation: &PartitionedRewardsCalculation,
        rewards_metrics: &mut RewardsMetrics,
        thread_pool: &ThreadPool,
    ) -> u64 {
        let RewardCommissionLamportAmounts {
            distributed_lamports,
            distributed_to_incinerator_lamports,
            burned_lamports,
        } = self.distribute_reward_commissions(
            parent_epoch,
            rewards_calculation,
            rewards_metrics,
            thread_pool,
        );

        let slot = self.slot();
        let distribution_starting_block_height =
            self.block_height() + REWARD_CALCULATION_NUM_BLOCKS;

        let PartitionedRewardsCalculation {
            stake_rewards,
            point_value,
            ..
        } = rewards_calculation;

        let stake_rewards = Arc::clone(&stake_rewards.stake_rewards);

        let num_partitions = self.get_reward_distribution_num_blocks(&stake_rewards);
        self.set_epoch_reward_status_calculation(distribution_starting_block_height, stake_rewards);

        self.create_epoch_rewards_sysvar(
            distributed_lamports + distributed_to_incinerator_lamports + burned_lamports,
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
        distributed_lamports
    }

    // Calculate rewards from previous epoch and distribute reward commissions
    pub(in crate::bank) fn calculate_rewards(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: Vec<(&Pubkey, &StakeAccount<Delegation>)>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
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
                Arc::new(self.calculate_rewards_for_partitioning(
                    stake_history,
                    stake_delegations,
                    cached_vote_accounts,
                    rewarded_epoch,
                    reward_calc_tracer,
                    thread_pool,
                    metrics,
                ))
            })
            .clone();
        drop(epoch_rewards_calculation_cache);

        rewards_calculation
    }

    /// Returns a tuple containing the total amount of commission lamports
    /// distributed and the total amount of lamports burned.
    pub(in crate::bank) fn distribute_reward_commissions(
        &mut self,
        prev_epoch: Epoch,
        rewards_calculation: &PartitionedRewardsCalculation,
        rewards_metrics: &mut RewardsMetrics,
        thread_pool: &ThreadPool,
    ) -> RewardCommissionLamportAmounts {
        let PartitionedRewardsCalculation {
            reward_commissions,
            stake_rewards,
            capitalization,
            point_value,
            num_filtered_vote_accounts,
            ..
        } = rewards_calculation;

        // Load the commission accounts and apply their rewards.
        // This is intentionally deferred from calculation time so that any
        // intervening account mutations (e.g. VAT burns in
        // `update_epoch_stakes`) are reflected.
        let (reward_commission_accounts, load_and_reward_commission_accounts_us) =
            measure_us!(self.load_and_reward_commission_accounts(reward_commissions, thread_pool));
        rewards_metrics.load_and_reward_commission_accounts_us =
            load_and_reward_commission_accounts_us;
        info!(
            "load_and_reward_commission_accounts: input_count={} output_count={} elapsed_us={}",
            reward_commissions.len(),
            reward_commission_accounts.accounts_with_rewards.len(),
            load_and_reward_commission_accounts_us,
        );

        let RewardCommissionLamportAmounts {
            distributed_lamports,
            distributed_to_incinerator_lamports,
            burned_lamports,
        } = reward_commission_accounts.amounts;
        self.store_commission_accounts_partitioned(&reward_commission_accounts, rewards_metrics);
        self.update_reward_commissions(&reward_commission_accounts);

        let StakeRewardCalculation {
            total_stake_rewards_lamports,
            ..
        } = stake_rewards;

        // verify that we didn't pay any more than we expected to
        assert!(
            point_value.rewards
                >= distributed_lamports
                    + distributed_to_incinerator_lamports
                    + burned_lamports
                    + total_stake_rewards_lamports
        );
        info!(
            "distributed reward commissions: {} out of {}, remaining {}",
            distributed_lamports + distributed_to_incinerator_lamports + burned_lamports,
            point_value.rewards,
            total_stake_rewards_lamports
        );

        let num_stake_accounts = self.stakes_cache.stakes().stake_delegations().len();
        let num_vote_accounts = *num_filtered_vote_accounts;
        self.capitalization.fetch_add(
            distributed_lamports + distributed_to_incinerator_lamports,
            Relaxed,
        );

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
            ("validator_rewards", distributed_lamports, i64),
            (
                "validator_rewards_to_incinerator",
                distributed_to_incinerator_lamports,
                i64
            ),
            ("validator_rewards_burned", burned_lamports, i64),
            ("active_stake", active_stake, i64),
            ("pre_capitalization", *capitalization, i64),
            ("post_capitalization", self.capitalization(), i64),
            ("num_stake_accounts", num_stake_accounts, i64),
            ("num_vote_accounts", num_vote_accounts, i64),
        );

        reward_commission_accounts.amounts
    }

    fn store_commission_accounts_partitioned(
        &self,
        reward_commission_accounts: &RewardCommissionAccounts,
        metrics: &RewardsMetrics,
    ) {
        let (_, measure_us) = measure_us!({
            let storable = RewardCommissionAccountsStorable {
                slot: self.slot(),
                reward_commission_accounts,
            };
            self.store_accounts(storable);
        });

        metrics
            .store_commission_accounts_us
            .fetch_add(measure_us, Relaxed);
    }

    /// Calculate rewards from previous epoch to prepare for partitioned distribution.
    pub(super) fn calculate_rewards_for_partitioning<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
        reward_calc_tracer: Option<impl Fn(&RewardCalculationEvent) + Send + Sync>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> PartitionedRewardsCalculation {
        let capitalization = self.capitalization();
        let epoch_inflation_rewards =
            self.calculate_epoch_inflation_rewards(capitalization, rewarded_epoch);
        // `distribution_epoch_vote_accounts` is the post-VAT-filter snapshot
        // produced upstream of this call (or unfiltered when VAT is off),
        // so its length is the right value for the `epoch_rewards` metric.
        let num_filtered_vote_accounts =
            cached_vote_accounts.distribution_epoch_vote_accounts.len();

        let CalculateValidatorRewardsResult {
            reward_commissions,
            stake_reward_calculation: stake_rewards,
            point_value,
        } = self
            .calculate_validator_rewards(
                stake_history,
                stake_delegations,
                cached_vote_accounts,
                rewarded_epoch,
                epoch_inflation_rewards,
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
            reward_commissions,
            stake_rewards,
            capitalization,
            point_value,
            num_filtered_vote_accounts,
        }
    }

    /// Calculate epoch reward and return stake rewards and commissions.
    fn calculate_validator_rewards<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
        epoch_inflation_rewards: u64,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        thread_pool: &ThreadPool,
        metrics: &mut RewardsMetrics,
    ) -> Option<CalculateValidatorRewardsResult> {
        let ag_epoch_type = self.get_alpenglow_epoch_type(rewarded_epoch);
        self.calculate_reward_points_partitioned(
            stake_history,
            &stake_delegations,
            &cached_vote_accounts,
            epoch_inflation_rewards,
            &ag_epoch_type,
            thread_pool,
            metrics,
        )
        .map(|point_value| {
            let (reward_commissions, stake_reward_calculation) = self
                .calculate_stake_rewards_and_commissions(
                    stake_history,
                    stake_delegations,
                    cached_vote_accounts,
                    rewarded_epoch,
                    point_value.clone(),
                    &ag_epoch_type,
                    thread_pool,
                    reward_calc_tracer,
                    metrics,
                );
            CalculateValidatorRewardsResult {
                reward_commissions,
                stake_reward_calculation,
                point_value,
            }
        })
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

        // Use the vote-account snapshot from epoch_stakes, which is VAT-filtered
        // when admission filtering is enabled. Recalculation should match the
        // vote-account admission policy used for distribution.
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

    #[expect(clippy::too_many_arguments)]
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
        commission_rate_in_basis_points: bool,
        adjust_delegations_for_rent: bool,
        ag_epoch_type: &AlpenglowEpochType,
        custom_commission_collector: bool,
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

        let current_lamports = stake_account.lamports();
        let minimum_lamports = self
            .rent_collector
            .rent
            .minimum_balance(stake_account.data_len());
        let mut stake = *stake_account.stake();

        let Some(vote_account) = distribution_epoch_vote_accounts.get(&vote_pubkey) else {
            debug!("could not find vote account {vote_pubkey} in cache");
            // Even if the vote account doesn't exist, there might still be a
            // need to adjust the stake delegation
            if adjust_delegations_for_rent {
                let delegation = stake.delegation.stake;
                let stake_was_adjusted = adjust_delegation_for_rent(
                    &mut stake.delegation,
                    rewarded_epoch,
                    delegation,
                    current_lamports,
                    minimum_lamports,
                );
                if stake_was_adjusted {
                    debug!("delegation for stake {stake_pubkey} was adjusted");
                    let stake_reward = PartitionedStakeReward {
                        stake_pubkey,
                        stake,
                        stake_reward: 0,
                        commission_bps: (!custom_commission_collector).then_some(0),
                    };
                    // Set `is_vote_account` to `false` in order to deliberately
                    // fail during commission collector checks. This avoids
                    // creating a reward entry during payout.
                    let reward_commission = RewardCommission {
                        commission_bps: (!custom_commission_collector).then_some(0),
                        commission_lamports: 0,
                        burned_lamports: 0,
                        is_vote_account: false,
                    };
                    return Some(DelegationRewards {
                        stake_reward,
                        commission_pubkey: vote_pubkey,
                        reward_commission,
                    });
                } else {
                    debug!("delegation for stake {stake_pubkey} was not adjusted");
                    return None;
                }
            } else {
                return None;
            }
        };
        let vote_state = vote_account.vote_state_view();

        // Fetch the voter commission from past epochs to attempt to
        // delay the effect of commission updates by at least one
        // full epoch.
        // When `commission_rate_in_basis_points` is true, use the new field
        // `inflation_rewards_commission_bps`; otherwise use the legacy
        // percentage field and convert to basis points by multiplying by 100.
        let commission_bps = if delay_commission_updates {
            let vote_state_for_commission = snapshot_epoch_vote_accounts
                .and_then(|eva| eva.get(&vote_pubkey))
                .or_else(|| rewarded_epoch_vote_accounts.and_then(|eva| eva.get(&vote_pubkey)))
                .map(|vote_account| vote_account.vote_state_view())
                .unwrap_or(vote_state);
            if commission_rate_in_basis_points {
                vote_state_for_commission.inflation_rewards_commission()
            } else {
                vote_state_for_commission.commission() as u16 * 100
            }
        } else if commission_rate_in_basis_points {
            vote_state.inflation_rewards_commission()
        } else {
            vote_state.commission() as u16 * 100
        };

        match redeem_rewards(
            stake,
            commission_bps,
            DelegatedVoteState::from(vote_state),
            CalculationEnvironment {
                rewarded_epoch,
                point_value,
                stake_history,
                new_rate_activation_epoch,
                commission_rate_in_basis_points,
                adjust_delegations_for_rent,
            },
            reward_calc_tracer,
            ag_epoch_type,
            &self.epoch_stakes,
            current_lamports,
            minimum_lamports,
        ) {
            Ok((stake_reward, commission_lamports, stake)) => {
                let stake_reward = PartitionedStakeReward {
                    stake_pubkey,
                    stake,
                    stake_reward,
                    commission_bps: (!custom_commission_collector).then_some(commission_bps),
                };
                let (commission_pubkey, is_vote_account) = if custom_commission_collector {
                    let commission_pubkey = *vote_state
                        .inflation_rewards_collector()
                        .unwrap_or(&vote_pubkey);
                    (commission_pubkey, commission_pubkey == vote_pubkey)
                } else {
                    (vote_pubkey, true)
                };
                let reward_commission = RewardCommission {
                    commission_bps: (!custom_commission_collector).then_some(commission_bps),
                    commission_lamports,
                    burned_lamports: 0,
                    is_vote_account,
                };
                Some(DelegationRewards {
                    stake_reward,
                    commission_pubkey,
                    reward_commission,
                })
            }
            Err(e) => {
                debug!("redeem_rewards() failed for {stake_pubkey}: {e:?}");
                None
            }
        }
    }

    /// Calculates epoch rewards for stake/commission accounts
    /// Returns commission accounts, stake rewards, and the sum of all stake rewards in lamports
    #[allow(clippy::too_many_arguments)]
    fn calculate_stake_rewards_and_commissions<'a>(
        &self,
        stake_history: &StakeHistory,
        stake_delegations: Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
        cached_vote_accounts: CachedVoteAccounts<'_>,
        rewarded_epoch: Epoch,
        point_value: PointValue,
        ag_epoch_type: &AlpenglowEpochType,
        thread_pool: &ThreadPool,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        metrics: &mut RewardsMetrics,
    ) -> (RewardCommissions, StakeRewardCalculation) {
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        let feature_snapshot = self.feature_set.snapshot();
        let delay_commission_updates = feature_snapshot.delay_commission_updates;
        let commission_rate_in_basis_points = feature_snapshot.commission_rate_in_basis_points;
        // Name intentionally doesn't match -- "adjust delegations for rent" is
        // part of relaxing post-exec min balance checks.
        let adjust_delegations_for_rent = feature_snapshot.relax_post_exec_min_balance_check;
        let custom_commission_collector = feature_snapshot.custom_commission_collector;

        let mut measure_redeem_rewards = Measure::start("redeem-rewards");
        // For N stake delegations, where N is >1,000,000, we produce:
        // * N stake rewards,
        // * M reward commission accounts, where M is a number of stake nodes.
        //   Currently, way smaller number than 1,000,000. And we can expect it
        //   to always be significantly smaller than number of delegations.
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
                .filter_map(|((stake_pubkey, stake_account), stake_reward_ref)| {
                    let maybe_reward_record = self.redeem_delegation_rewards(
                        rewarded_epoch,
                        stake_pubkey,
                        stake_account,
                        &point_value,
                        stake_history,
                        &cached_vote_accounts,
                        reward_calc_tracer.as_ref(),
                        new_warmup_cooldown_rate_epoch,
                        delay_commission_updates,
                        commission_rate_in_basis_points,
                        adjust_delegations_for_rent,
                        ag_epoch_type,
                        custom_commission_collector,
                    );

                    let (stake_reward, maybe_reward_record) = match maybe_reward_record {
                        Some(res) => {
                            let DelegationRewards {
                                stake_reward,
                                commission_pubkey,
                                reward_commission,
                            } = res;
                            let stakers_reward = stake_reward.stake_reward;
                            (
                                Some(stake_reward),
                                Some((stakers_reward, commission_pubkey, reward_commission)),
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
                    |mut rewards_accumulator,
                     (stakers_reward, commission_pubkey, reward_commission)| {
                        rewards_accumulator.add_reward(
                            commission_pubkey,
                            reward_commission,
                            stakers_reward,
                        );
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
            reward_commissions,
            num_stake_rewards,
            total_stake_rewards_lamports,
        } = rewards_accumulator;
        // SAFETY: We initialized all the `stake_rewards` elements up to the capacity.
        unsafe {
            stake_rewards.assume_init(num_stake_rewards);
        }
        measure_redeem_rewards.stop();
        metrics.redeem_rewards_us = measure_redeem_rewards.as_us();

        (
            reward_commissions,
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
        stake_delegations: &Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
        cached_vote_accounts: &CachedVoteAccounts<'_>,
        epoch_inflation_rewards: u64,
        ag_epoch_type: &AlpenglowEpochType,
        thread_pool: &ThreadPool,
        metrics: &RewardsMetrics,
    ) -> Option<PointValue> {
        let CachedVoteAccounts {
            distribution_epoch_vote_accounts,
            ..
        } = cached_vote_accounts;

        let solana_vote_program: Pubkey = solana_vote_program::id();
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        match ag_epoch_type {
            AlpenglowEpochType::Alpenglow { .. } => {
                // In alpenglow, we do not need to compute `PointValue::points` as the final
                // rewards are simply the total credits stored in the vote account.  We just need
                // to return a `Some` value with valid rewards.
                return Some(PointValue {
                    rewards: epoch_inflation_rewards,
                    points: 0,
                });
            }
            AlpenglowEpochType::Tower => {
                // For tower we need to compute the valid `PointValue::points`.
            }
            AlpenglowEpochType::MigrationEpoch { .. } => {
                // For the migrating epoch, we need to compute the tower portion of `PointValue::points`.
            }
        }

        let (points, measure_us) = measure_us!(thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .map(|(_stake_pubkey, stake_account)| {
                    let vote_pubkey = stake_account.delegation().voter_pubkey;

                    let Some(vote_account) = distribution_epoch_vote_accounts.get(&vote_pubkey)
                    else {
                        return 0;
                    };
                    if vote_account.owner() != &solana_vote_program {
                        return 0;
                    }

                    calculate_points_for_tower(
                        stake_account.stake_state(),
                        DelegatedVoteState::from(vote_account.vote_state_view()),
                        stake_history,
                        new_warmup_cooldown_rate_epoch,
                        &self.epoch_stakes,
                    )
                    .unwrap_or(0)
                })
                .sum::<u128>()
        }));
        metrics.calculate_points_us.fetch_add(measure_us, Relaxed);

        (points > 0).then_some(PointValue {
            rewards: epoch_inflation_rewards,
            points,
        })
    }

    /// If rewards are still active, recalculates partitioned stake rewards and
    /// updates Bank::epoch_reward_status. This method assumes that reward
    /// commissions have already been calculated and delivered, and *only*
    /// recalculates stake rewards
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
            let (stake_rewards, partition_indices) =
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
    ) -> (Arc<PartitionedStakeRewards>, Vec<Vec<usize>>) {
        assert!(epoch_rewards_sysvar.active);
        // If rewards are active, the rewarded epoch is always the immediately
        // preceding epoch.
        let rewarded_epoch = self.epoch().saturating_sub(1);
        let ag_epoch_type = self.get_alpenglow_epoch_type(rewarded_epoch);

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
        // field is relevant. It is assumed that reward commission accounts have
        // already been calculated and delivered, while
        // `StakeRewardCalculation::total_rewards` only reflects rewards that
        // have not yet been distributed.
        //
        // NOTE: the `RewardCommissionAccounts` will NOT have a correct
        // post_lamport amount if the commission account is NOT the vote account,
        // because the commission account is loaded from the current bank, and
        // not the start of the epoch. We don't have a snapshot of all commission
        // accounts from the start of the epoch. For this reason, the
        // `RewardCommissionAccounts` calculated in this function call should
        // NOT be used ever.
        let (_, StakeRewardCalculation { stake_rewards, .. }) = self
            .calculate_stake_rewards_and_commissions(
                &stake_history,
                stake_delegations,
                cached_vote_accounts,
                rewarded_epoch,
                point_value,
                &ag_epoch_type,
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
        (stake_rewards, partition_indices)
    }

    /// Load each planned commission account from the store and apply its
    /// reward. This is the single point where commission account data is
    /// fetched, ensuring we always see the latest balances — including any
    /// intervening account mutations (e.g. VAT burns in `update_epoch_stakes`)
    /// that happen between calculation and distribution.
    fn load_and_reward_commission_accounts(
        &self,
        reward_commissions: &RewardCommissions,
        thread_pool: &ThreadPool,
    ) -> RewardCommissionAccounts {
        let reserved_account_keys = &self.reserved_account_keys;
        let rent = &self.rent_collector().rent;
        let feature_snapshot = self.feature_set.snapshot();
        let relax_post_exec_min_balance_check = feature_snapshot.relax_post_exec_min_balance_check;
        let custom_commission_collector = feature_snapshot.custom_commission_collector;
        let total_non_incinerator_burned_lamports = AtomicU64::new(0);
        let total_incinerator_lamports = AtomicU64::new(0);

        let accounts_with_rewards: Vec<_> = thread_pool.install(|| {
            reward_commissions
                .par_iter()
                .filter_map(
                    |(
                        commission_pubkey,
                        RewardCommission {
                            commission_bps,
                            commission_lamports,
                            burned_lamports,
                            is_vote_account,
                        },
                    )| {
                        let maybe_commission_account =
                            self.get_account_with_fixed_root_no_cache(commission_pubkey);
                        let mut commission_account = if custom_commission_collector {
                            // If the account doesn't exist, the vote commission
                            // may be enough lamports to cover rent-exemption
                            // and properly create the commission account.
                            maybe_commission_account.unwrap_or_default()
                        } else {
                            // Before SIMD-0232, commission accounts were always
                            // vote accounts, which cannot be closed unless the
                            // account hasn't voted for at least a full epoch.
                            // This means that `maybe_commission_account` should
                            // always exist.
                            let Some(commission_account) = maybe_commission_account else {
                                debug!(
                                    "commission account {commission_pubkey} missing at \
                                     distribution time"
                                );
                                return None;
                            };
                            commission_account
                        };
                        if *burned_lamports != 0 {
                            total_non_incinerator_burned_lamports
                                .fetch_add(*burned_lamports, Relaxed);
                        }
                        let pre_lamports = commission_account.lamports();
                        if let Err(err) =
                            commission_account.checked_add_lamports(*commission_lamports)
                        {
                            debug!("reward redemption failed for {commission_pubkey}: {err:?}");
                            total_non_incinerator_burned_lamports
                                .fetch_add(*commission_lamports, Relaxed);
                            return None;
                        }
                        if !is_vote_account {
                            match Self::collector_type_checked(
                                commission_pubkey,
                                pre_lamports,
                                &commission_account,
                                reserved_account_keys,
                                rent,
                                relax_post_exec_min_balance_check,
                            ) {
                                Ok(ExternalCollectorType::SystemAccount) => {}
                                Ok(ExternalCollectorType::Incinerator) => {
                                    total_incinerator_lamports
                                        .fetch_add(*commission_lamports, Relaxed);
                                }
                                Err(err) => {
                                    debug!(
                                        "reward redemption failed for {commission_pubkey} due to \
                                         commission account error: {err:?}"
                                    );
                                    total_non_incinerator_burned_lamports
                                        .fetch_add(*commission_lamports, Relaxed);
                                    return None;
                                }
                            }
                        }
                        Some((
                            *commission_pubkey,
                            RewardInfo {
                                reward_type: RewardType::Voting,
                                lamports: *commission_lamports as i64,
                                post_balance: commission_account.lamports(),
                                commission_bps: *commission_bps,
                            },
                            commission_account,
                        ))
                    },
                )
                .collect()
        });

        let distributed_to_incinerator_lamports = total_incinerator_lamports.into_inner();
        let distributed_lamports = accounts_with_rewards
            .iter()
            .map(|(_, info, _)| info.lamports as u64)
            .sum::<u64>()
            .checked_sub(distributed_to_incinerator_lamports)
            .expect("incinerator lamports must be a subset of all distributed lamports");
        RewardCommissionAccounts {
            accounts_with_rewards,
            amounts: RewardCommissionLamportAmounts {
                distributed_lamports,
                distributed_to_incinerator_lamports,
                burned_lamports: total_non_incinerator_burned_lamports.into_inner(),
            },
        }
    }

    fn update_reward_commissions(&self, reward_commission_accounts: &RewardCommissionAccounts) {
        let mut rewards = self.rewards.write().unwrap();
        rewards.reserve(reward_commission_accounts.accounts_with_rewards.len());
        reward_commission_accounts
            .accounts_with_rewards
            .iter()
            .for_each(|(commission_pubkey, reward_commission, _)| {
                rewards.push((*commission_pubkey, *reward_commission));
            });
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::{
                RewardInfo, SlotLeader, null_tracer,
                partitioned_epoch_rewards::{
                    EpochRewardPhase, EpochRewardStatus, PartitionedStakeRewards,
                    StartBlockHeightAndPartitionedRewards,
                    tests::{
                        RewardBank, SLOTS_PER_EPOCH, build_partitioned_stake_rewards,
                        create_default_reward_bank, create_reward_bank,
                        create_reward_bank_with_specific_stakes, populate_vote_accounts_with_votes,
                    },
                },
                tests::create_genesis_config,
            },
            bank_forks::BankForks,
            genesis_utils::{self, GenesisConfigInfo, deactivate_features},
            stake_account::StakeAccount,
            stake_utils,
            stakes::{Stakes, tests::create_staked_node_accounts},
        },
        agave_feature_set::{FeatureSet, delay_commission_updates},
        agave_votor_messages::consensus_message::BLS_KEYPAIR_DERIVE_SEED,
        rand::Rng,
        rayon::ThreadPoolBuilder,
        solana_account::{
            AccountSharedData, ReadableAccount, accounts_equal, state_traits::StateMut,
        },
        solana_accounts_db::partitioned_rewards::PartitionedEpochRewardsConfig,
        solana_bls_signatures::keypair::Keypair as BLSKeypair,
        solana_clock::Clock,
        solana_epoch_schedule::EpochSchedule,
        solana_keypair::Keypair,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_rent::Rent,
        solana_sdk_ids::incinerator,
        solana_signer::Signer,
        solana_stake_interface::{
            stake_flags::StakeFlags,
            state::{Authorized, Delegation, Meta, Stake, StakeStateV2},
        },
        solana_vote_interface::state::{
            BLS_PUBLIC_KEY_COMPRESSED_SIZE, VoteInitV2, VoteStateV4, VoteStateVersions,
        },
        solana_vote_program::vote_state::{self, create_bls_proof_of_possession},
        std::{
            collections::HashSet,
            sync::{Arc, RwLock, RwLockReadGuard},
        },
        test_case::{test_case, test_matrix},
    };

    #[test]
    fn test_store_commission_accounts_partitioned() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let num_reward_commissions = 100;
        let mut rng = rand::rng();
        let entries: Vec<(Pubkey, RewardInfo, AccountSharedData)> = (0..num_reward_commissions)
            .map(|_| {
                let commission_balance = rng.random_range(1..200);
                let commission_bps: u16 = rng.random_range(100..2_000);
                let commission_lamports: u64 = rng.random_range(1..200);
                let mut commission_account = AccountSharedData::default();
                commission_account.set_lamports(commission_balance);
                let info = RewardInfo {
                    reward_type: RewardType::Voting,
                    lamports: commission_lamports as i64,
                    post_balance: commission_lamports,
                    commission_bps: Some(commission_bps),
                };
                (Pubkey::new_unique(), info, commission_account)
            })
            .collect();

        let mut reward_commission_accounts = RewardCommissionAccounts::default();
        for (commission_pubkey, info, commission_account) in &entries {
            reward_commission_accounts.accounts_with_rewards.push((
                *commission_pubkey,
                *info,
                commission_account.clone(),
            ));
            reward_commission_accounts.amounts.distributed_lamports += info.lamports as u64;
        }

        let metrics = RewardsMetrics::default();

        let total_reward_commissions = reward_commission_accounts.amounts.distributed_lamports;
        bank.store_commission_accounts_partitioned(&reward_commission_accounts, &metrics);
        assert_eq!(
            num_reward_commissions,
            reward_commission_accounts.accounts_with_rewards.len()
        );
        assert_eq!(
            entries
                .iter()
                .map(|(_, info, _)| info.lamports as u64)
                .sum::<u64>(),
            total_reward_commissions
        );

        // load accounts to make sure they were stored correctly
        for (commission_pubkey, _, commission_account) in &entries {
            let loaded_account = bank
                .load_slow_with_fixed_root(&bank.ancestors, commission_pubkey)
                .unwrap();
            assert!(accounts_equal(&loaded_account.0, commission_account));
        }
    }

    #[test]
    fn test_store_commission_accounts_partitioned_empty() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);

        let expected = 0;
        let reward_commission_accounts = RewardCommissionAccounts::default();
        let metrics = RewardsMetrics::default();
        let total_reward_commissions = reward_commission_accounts.amounts.distributed_lamports;

        bank.store_commission_accounts_partitioned(&reward_commission_accounts, &metrics);
        assert_eq!(
            expected,
            reward_commission_accounts.accounts_with_rewards.len()
        );
        assert_eq!(0, total_reward_commissions);
    }

    #[test]
    /// Test rewards computation and partitioned rewards distribution at the epoch boundary
    fn test_rewards_computation() {
        agave_logger::setup();

        // Delegations to get rewards (2 SOL).
        let delegations = 100;
        let stakes = (0..delegations).map(|_| 2_000_000_000).collect::<Vec<_>>();
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
        let rewarded_epoch = 0;
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);
        let calculated_rewards = bank.calculate_validator_rewards(
            &stake_history,
            stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            expected_rewards,
            null_tracer(),
            &thread_pool,
            &mut rewards_metrics,
        );

        let reward_commissions = &calculated_rewards.as_ref().unwrap().reward_commissions;
        let stake_rewards = &calculated_rewards
            .as_ref()
            .unwrap()
            .stake_reward_calculation;

        let total_reward_commissions: u64 = reward_commissions
            .values()
            .map(|rc| rc.commission_lamports)
            .sum();

        // assert that total rewards matches the sum of reward commissions and stake rewards
        assert_eq!(
            stake_rewards.total_stake_rewards_lamports + total_reward_commissions,
            expected_rewards
        );

        // assert that number of stake rewards matches
        assert_eq!(stake_rewards.stake_rewards.num_rewards(), delegations);
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
            &AlpenglowEpochType::Tower,
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
            &AlpenglowEpochType::Tower,
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
        expect_reward: bool,
        // Additional lamport amount to ignore in collector account, used for
        // VAT burns in Alpenglow when the incinerator is an inflation collector
        extra_reward_lamport_amount: Option<u64>,
        // ops to perform before epoch ends
        create_with_balance: Option<u64>,
        delegate_stake_amount: Option<u64>,
        new_commission: Option<u8>,
        earned_credits: Option<u64>,
        new_inflation_rewards_collector: Option<Pubkey>,
    }

    fn recalculate_reward_commissions_for_tests(bank: &Bank) -> RewardCommissions {
        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        assert!(epoch_rewards_sysvar.active);
        let rewarded_epoch = bank.epoch().saturating_sub(1);

        let point_value = PointValue {
            rewards: epoch_rewards_sysvar.total_rewards,
            points: epoch_rewards_sysvar.total_points,
        };

        let stakes = bank.stakes_cache.stakes();
        let EpochRewardCalculateParamInfo {
            stake_history,
            stake_delegations,
            cached_vote_accounts,
        } = bank.get_epoch_params_for_recalculation(rewarded_epoch, &stakes);

        let (reward_commissions, ..) = bank.calculate_stake_rewards_and_commissions(
            &stake_history,
            stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            point_value,
            &AlpenglowEpochType::Tower,
            &thread_pool,
            null_tracer(),
            &mut RewardsMetrics::default(), // This is required, but not reporting anything at the moment
        );
        reward_commissions
    }

    fn recalculate_reward_commission_for_tests(
        bank: &Bank,
        commission_pubkey: &Pubkey,
    ) -> Option<RewardInfo> {
        let reward_commissions = recalculate_reward_commissions_for_tests(bank);
        reward_commissions.get(commission_pubkey).and_then(|rc| {
            let commission_account = bank.get_account(commission_pubkey).unwrap_or_default();
            // In the recalculation path, commissions have already been
            // distributed — so the current account balance already includes
            // them. We report that balance as the post_balance.
            let post_balance = commission_account.lamports();

            // This is artificial, but mimics the actual distribution logic a
            // bit better
            if *commission_account.owner() != solana_vote_program::id()
                && Bank::collector_type_checked(
                    commission_pubkey,
                    post_balance.saturating_sub(rc.commission_lamports),
                    &commission_account,
                    &bank.reserved_account_keys,
                    &bank.rent_collector().rent,
                    true,
                )
                .is_err()
            {
                None
            } else {
                Some(RewardInfo {
                    reward_type: RewardType::Voting,
                    lamports: rc.commission_lamports as i64,
                    post_balance,
                    commission_bps: rc.commission_bps,
                })
            }
        })
    }

    fn create_stake_account(
        lamports: u64,
        delegation: u64,
        vote_address: &Pubkey,
        activation_epoch: Epoch,
    ) -> AccountSharedData {
        let mut stake_account = AccountSharedData::new(
            lamports,
            StakeStateV2::size_of(),
            &solana_sdk_ids::stake::id(),
        );
        let rent_exempt_reserve = lamports.saturating_sub(delegation);

        let meta = Meta {
            authorized: Authorized::auto(&Pubkey::new_unique()),
            #[expect(deprecated)]
            rent_exempt_reserve,
            ..Meta::default()
        };

        let stake = Stake {
            delegation: Delegation::new(vote_address, delegation, activation_epoch),
            credits_observed: 0,
        };

        stake_account
            .set_state(&StakeStateV2::Stake(meta, stake, StakeFlags::empty()))
            .expect("set_state");
        stake_account
    }

    fn apply_epoch_operations(
        bank: Arc<Bank>,
        bank_forks: &RwLock<BankForks>,
        op: EpochOperations,
    ) -> Arc<Bank> {
        assert_eq!(bank.epoch(), op.epoch);
        for (vote_address, vote_op) in &op.vote_operations {
            if let Some(balance) = &vote_op.create_with_balance {
                // Create a BLS pubkey so the vote account passes VAT filtering
                let identity = Keypair::new();
                let bls_keypair =
                    BLSKeypair::derive_from_signer(&identity, BLS_KEYPAIR_DERIVE_SEED).unwrap();
                let (bls_pubkey, bls_pop) =
                    create_bls_proof_of_possession(vote_address, &bls_keypair);
                let vote_init = VoteInitV2 {
                    node_pubkey: identity.pubkey(),
                    authorized_voter: identity.pubkey(),
                    authorized_voter_bls_pubkey: bls_pubkey,
                    authorized_voter_bls_proof_of_possession: bls_pop,
                    ..VoteInitV2::default()
                };
                let vote_state = VoteStateV4::new(
                    &vote_init,
                    vote_address,
                    &identity.pubkey(),
                    &Clock::default(),
                );
                let mut account = solana_account::AccountSharedData::new(
                    *balance,
                    VoteStateV4::size_of(),
                    &solana_vote_program::id(),
                );
                account
                    .serialize_data(&VoteStateVersions::new_v4(vote_state))
                    .unwrap();
                bank.store_account(vote_address, &account);
            }

            if let Some(stake_amount) = &vote_op.delegate_stake_amount {
                let size = StakeStateV2::size_of();
                let rent_exempt_reserve = bank.rent_collector().rent.minimum_balance(size);
                let lamports = rent_exempt_reserve + stake_amount;
                let stake_account =
                    create_stake_account(lamports, *stake_amount, vote_address, bank.epoch());
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

            if let Some(inflation_rewards_collector) = vote_op.new_inflation_rewards_collector {
                modify_vote_state(&|vote_state: &mut VoteStateV4| {
                    vote_state.inflation_rewards_collector = inflation_rewards_collector;
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
        let slot = bank.slot() + SLOTS_PER_EPOCH;
        let prev_bank = bank.clone();
        let bank =
            Bank::new_from_parent_with_bank_forks(bank_forks, bank, SlotLeader::new_unique(), slot);

        for (vote_address, vote_op) in &op.vote_operations {
            // some tests delegate before the vote account exists
            let collector_address = bank
                .get_account(vote_address)
                .map(|vote_account| {
                    VoteStateV4::deserialize(vote_account.data(), vote_address)
                        .unwrap()
                        .inflation_rewards_collector
                })
                .unwrap_or(*vote_address);
            let recalculated_vote_reward =
                recalculate_reward_commission_for_tests(&bank, &collector_address);
            let vote_reward = bank
                .rewards
                .read()
                .unwrap()
                .iter()
                .find(|(address, _reward)| *address == collector_address)
                .map(|(_address, reward)| *reward);

            let prev_collector_balance = prev_bank
                .get_account(&collector_address)
                .unwrap_or_default()
                .lamports();
            let collector_balance = bank.get_balance(&collector_address);

            if vote_op.expect_reward {
                let reward_lamports = collector_balance
                    - prev_collector_balance
                    - vote_op.extra_reward_lamport_amount.unwrap_or(0);
                let expected_vote_reward = RewardInfo {
                    reward_type: RewardType::Voting,
                    lamports: reward_lamports as i64,
                    post_balance: collector_balance,
                    commission_bps: None,
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

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let vote_address = Pubkey::new_unique();

        // No reward should be given in the epoch that a vote account is
        // delegated to for the first time
        let mut bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
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
        bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(2),
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 2,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(3),
                        earned_credits: Some(1000),
                        expect_reward: true,
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
        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        assert!(bank.feature_set.snapshot().delay_commission_updates);

        let vote_address = Pubkey::new_unique();
        let mut bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
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
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        create_with_balance: Some(LAMPORTS_PER_SOL),
                        new_commission: Some(1),
                        earned_credits: Some(1000),
                        expect_reward: true,
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
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 2,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(2),
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 3,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        new_commission: Some(3),
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );
    }

    #[test]
    fn test_adjust_delegation_for_prestaked_vote_account() {
        let GenesisConfigInfo {
            mut genesis_config,
            voting_keypair,
            ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );
        let genesis_vote_address = voting_keypair.pubkey();

        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);
        genesis_config.rent = Rent::default();

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();

        let old_delegation = LAMPORTS_PER_SOL;
        let rent_exempt_amount = genesis_config.rent.minimum_balance(StakeStateV2::size_of());

        let vote_address = Pubkey::new_unique();

        // No rent exemption at all
        let stake_address_to_adjust = Pubkey::new_unique();
        let stake_account =
            create_stake_account(old_delegation, old_delegation, &vote_address, bank.epoch());
        bank.store_account(&stake_address_to_adjust, &stake_account);

        // Will be deactivated, below rent exemption
        let stake_address_to_deactivate = Pubkey::new_unique();
        let stake_account = create_stake_account(
            rent_exempt_amount - 1,
            rent_exempt_amount - 1,
            &vote_address,
            bank.epoch(),
        );
        bank.store_account(&stake_address_to_deactivate, &stake_account);

        // Make a vote account earn points, otherwise stakes don't get updated
        // at all
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // Advance bank to next slot for distribution, see adjustment
        let slot = bank.slot();
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::new_unique(),
            slot + 1,
        );

        let stake_account = bank.get_account(&stake_address_to_adjust).unwrap();
        let stake_state: StakeStateV2 = stake_account.state().unwrap();
        let new_delegation = stake_state.stake().unwrap().delegation.stake;
        assert_ne!(old_delegation, new_delegation);
        assert_eq!(old_delegation, new_delegation + rent_exempt_amount);

        let stake_account = bank.get_account(&stake_address_to_deactivate).unwrap();
        let stake_state: StakeStateV2 = stake_account.state().unwrap();
        let new_delegation = stake_state.stake().unwrap().delegation;
        assert_eq!(new_delegation.stake, 0);
        assert_eq!(new_delegation.deactivation_epoch, 0);
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
        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        assert!(bank.feature_set.snapshot().delay_commission_updates);

        let genesis_vote_address = voting_keypair.pubkey();

        // Check that staked genesis vote accounts use the initial commission
        // rate for the first reward epoch
        let mut bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        new_commission: Some(1),
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // Check that staked genesis vote accounts use the initial commission
        // rate for the second reward epoch too.
        bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        new_commission: Some(2),
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 2,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 3,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 4,
                vote_operations: vec![(
                    genesis_vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expect_reward: true,
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
        let (vote_rewards_accounts, stake_reward_calculation) = bank
            .calculate_stake_rewards_and_commissions(
                &stake_history,
                stake_delegations,
                cached_vote_accounts,
                rewarded_epoch,
                point_value,
                &AlpenglowEpochType::Tower,
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

        assert_eq!(vote_rewards_accounts.len(), 1);
        let reward_commission = vote_rewards_accounts.get(vote_pubkey).unwrap();
        let vote_rewards = 0;
        assert_eq!(reward_commission.commission_lamports, vote_rewards);
        assert_eq!(reward_commission.commission_bps, None);

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
                commission_bps: None,
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
        let (RewardBank { bank, .. }, bank_forks) = create_reward_bank(
            expected_num_delegations,
            num_rewards_per_block,
            SLOTS_PER_EPOCH,
        );
        let rewarded_epoch = bank.epoch() - 1;

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
            stake_delegations,
            cached_vote_accounts,
            rewarded_epoch,
            null_tracer(),
            &thread_pool,
            &mut rewards_metrics,
        );
        drop(stakes);

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let (recalculated_rewards, recalculated_partition_indices) =
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
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            new_slot,
        );

        let epoch_rewards_sysvar = bank.get_epoch_rewards_sysvar();
        let (recalculated_rewards, recalculated_partition_indices) =
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

        // Advance until reward distribution has completed.
        let mut bank = bank;
        for _ in 1..bank.get_epoch_rewards_sysvar().num_partitions {
            assert!(bank.get_epoch_rewards_sysvar().active);
            let new_slot = bank.slot() + 1;
            bank = Arc::new(Bank::new_from_parent(bank, SlotLeader::default(), new_slot));
        }

        assert!(!bank.get_epoch_rewards_sysvar().active);
    }

    #[test]
    fn test_recalculate_partitioned_rewards() {
        let expected_num_delegations = 4;
        let num_rewards_per_block = 2;
        // Distribute 4 rewards over 2 blocks
        let mut stakes = vec![2_000_000_000; expected_num_delegations - 1];
        // Add stake large enough to be affected by total-rewards discrepancy
        stakes.push(40_000_000_000);
        let (RewardBank { bank, .. }, _bank_forks) = create_reward_bank_with_specific_stakes(
            stakes,
            num_rewards_per_block,
            SLOTS_PER_EPOCH - 1,
        );
        let rewarded_epoch = bank.epoch();

        // Advance to next epoch boundary to update EpochStakes Kludgy because
        // mutable Bank methods require the bank not be Arc-wrapped.
        let new_slot = bank.slot() + 1;
        let mut bank = Bank::new_from_parent(bank, SlotLeader::default(), new_slot);
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
            stake_delegations,
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

        // Advance to first distribution slot (bank_forks kept in scope so parent has fork_graph)
        let mut bank =
            Bank::new_from_parent(Arc::new(bank), SlotLeader::default(), SLOTS_PER_EPOCH + 1);

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

        // Advance until reward distribution has completed.
        let mut bank = bank;
        for _ in 1..bank.get_epoch_rewards_sysvar().num_partitions {
            assert!(bank.get_epoch_rewards_sysvar().active);
            let next_slot = bank.slot() + 1;
            bank = Bank::new_from_parent(Arc::new(bank), SlotLeader::default(), next_slot);
            bank.recalculate_partitioned_rewards_if_active(|| &thread_pool);
        }

        assert_eq!(bank.epoch_reward_status, EpochRewardStatus::Inactive);
    }

    #[test]
    fn test_initialize_after_snapshot_restore() {
        let expected_num_stake_rewards = 4;
        let num_rewards_per_block = 2;
        // Distribute 4 rewards over 2 blocks
        let stakes = vec![
            100_000_000,   // valid delegation
            2_000_000_000, // valid delegation
            3_000_000_000, // valid delegation
            4_000_000_000, // valid delegation
        ];
        let (RewardBank { bank, .. }, bank_forks) = create_reward_bank_with_specific_stakes(
            stakes,
            num_rewards_per_block,
            SLOTS_PER_EPOCH - 1,
        );

        // Advance to next epoch boundary (bank_forks kept in scope so parent has fork_graph)
        let new_slot = bank.slot() + 1;
        let mut bank = Bank::new_from_parent(bank, SlotLeader::default(), new_slot);

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
        let _ = &bank_forks; // Keep in scope so parent banks retain fork_graph
    }

    #[test]
    fn test_initialize_after_snapshot_restore_preserves_vat_filtered_rewards() {
        let num_validators = crate::bank::MAX_ALPENGLOW_VOTE_ACCOUNTS + 1;
        let num_rewards_per_block = 64;
        // Use unique stakes so VAT filtering deterministically excludes exactly
        // the lowest-staked validator instead of dropping an entire tie group.
        let stakes = (0..num_validators)
            .map(|index| 2_000_000_000 + (num_validators - index) as u64)
            .collect::<Vec<_>>();
        let (
            RewardBank {
                bank,
                voters,
                stakers,
            },
            bank_forks,
        ) = create_reward_bank_with_specific_stakes(
            stakes,
            num_rewards_per_block,
            SLOTS_PER_EPOCH - 1,
        );

        let filtered_vote_pubkey = *voters.last().unwrap();
        let filtered_stake_pubkey = *stakers.last().unwrap();

        // Advance to the epoch boundary, which computes the original in-memory
        // reward list and updates EpochStakes for the new epoch.
        let new_slot = bank.slot() + 1;
        let mut bank = Bank::new_from_parent(bank, SlotLeader::default(), new_slot);

        let leader_schedule_epoch = bank.epoch_schedule().get_leader_schedule_epoch(bank.slot());
        let filtered_epoch_vote_accounts = bank
            .epoch_stakes(leader_schedule_epoch)
            .unwrap()
            .stakes()
            .vote_accounts();
        assert_eq!(
            bank.stakes_cache.stakes().vote_accounts().len(),
            num_validators
        );
        assert_eq!(
            filtered_epoch_vote_accounts.len(),
            crate::bank::MAX_ALPENGLOW_VOTE_ACCOUNTS
        );
        assert!(
            filtered_epoch_vote_accounts
                .get(&filtered_vote_pubkey)
                .is_none()
        );

        let EpochRewardStatus::Active(EpochRewardPhase::Calculation(calculation_status)) =
            bank.epoch_reward_status.clone()
        else {
            panic!("{:?} not active calculation", bank.epoch_reward_status);
        };
        assert_eq!(
            calculation_status.all_stake_rewards.num_rewards(),
            crate::bank::MAX_ALPENGLOW_VOTE_ACCOUNTS
        );
        assert!(
            calculation_status
                .all_stake_rewards
                .enumerated_rewards_iter()
                .all(|(_, reward)| reward.stake_pubkey != filtered_stake_pubkey)
        );

        // Simulate snapshot restore: re-apply features from accounts and
        // rebuild epoch_reward_status from snapshot-stable state.
        bank.feature_set = Arc::new(FeatureSet::default());
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        bank.initialize_after_snapshot_restore(|| &thread_pool);

        let EpochRewardStatus::Active(EpochRewardPhase::Distribution(distribution_status)) =
            bank.epoch_reward_status.clone()
        else {
            panic!("{:?} not active distribution", bank.epoch_reward_status);
        };
        assert_eq!(
            distribution_status.all_stake_rewards.num_rewards(),
            crate::bank::MAX_ALPENGLOW_VOTE_ACCOUNTS
        );
        assert!(
            distribution_status
                .all_stake_rewards
                .enumerated_rewards_iter()
                .all(|(_, reward)| reward.stake_pubkey != filtered_stake_pubkey)
        );

        assert_eq!(
            calculation_status.distribution_starting_block_height,
            distribution_status.distribution_starting_block_height
        );
        assert_eq!(
            calculation_status.all_stake_rewards,
            distribution_status.all_stake_rewards
        );
        let _ = &bank_forks; // Keep in scope so parent banks retain fork_graph
    }

    #[test]
    fn test_reward_accumulator() {
        let mut accumulator1 = RewardsAccumulator::default();
        let mut accumulator2 = RewardsAccumulator::default();

        let commission_pubkey_a = Pubkey::new_unique();
        let commission_pubkey_b = Pubkey::new_unique();
        let commission_pubkey_c = Pubkey::new_unique();

        accumulator1.add_reward(
            commission_pubkey_a,
            RewardCommission {
                commission_bps: Some(1_000),
                commission_lamports: 50,
                burned_lamports: 0,
                is_vote_account: true,
            },
            50,
        );
        accumulator1.add_reward(
            commission_pubkey_b,
            RewardCommission {
                commission_bps: Some(1_000),
                commission_lamports: 50,
                burned_lamports: 0,
                is_vote_account: true,
            },
            50,
        );
        accumulator2.add_reward(
            commission_pubkey_b,
            RewardCommission {
                commission_bps: Some(1_000),
                commission_lamports: 30,
                burned_lamports: 0,
                is_vote_account: true,
            },
            30,
        );
        accumulator2.add_reward(
            commission_pubkey_c,
            RewardCommission {
                commission_bps: Some(1_000),
                commission_lamports: 50,
                burned_lamports: 0,
                is_vote_account: true,
            },
            50,
        );

        assert_eq!(accumulator1.num_stake_rewards, 2);
        assert_eq!(accumulator1.total_stake_rewards_lamports, 100);
        let reward_commission_a_1 = accumulator1
            .reward_commissions
            .get(&commission_pubkey_a)
            .unwrap();
        assert_eq!(reward_commission_a_1.commission_bps, Some(1_000));
        assert_eq!(reward_commission_a_1.commission_lamports, 50);

        let reward_commission_b_1 = accumulator1
            .reward_commissions
            .get(&commission_pubkey_b)
            .unwrap();
        assert_eq!(reward_commission_b_1.commission_bps, Some(1_000));
        assert_eq!(reward_commission_b_1.commission_lamports, 50);

        let reward_commission_b_2 = accumulator2
            .reward_commissions
            .get(&commission_pubkey_b)
            .unwrap();
        assert_eq!(reward_commission_b_2.commission_bps, Some(1_000));
        assert_eq!(reward_commission_b_2.commission_lamports, 30);

        let reward_commission_c_2 = accumulator2
            .reward_commissions
            .get(&commission_pubkey_c)
            .unwrap();
        assert_eq!(reward_commission_c_2.commission_bps, Some(1_000));
        assert_eq!(reward_commission_c_2.commission_lamports, 50);

        let accumulator = accumulator1.accumulate_into_larger(accumulator2);

        assert_eq!(accumulator.num_stake_rewards, 4);
        assert_eq!(accumulator.total_stake_rewards_lamports, 180);
        let reward_commission_a = accumulator
            .reward_commissions
            .get(&commission_pubkey_a)
            .unwrap();
        assert_eq!(reward_commission_a.commission_bps, Some(1_000));
        assert_eq!(reward_commission_a.commission_lamports, 50);

        let reward_commission_b = accumulator
            .reward_commissions
            .get(&commission_pubkey_b)
            .unwrap();
        assert_eq!(reward_commission_b.commission_bps, Some(1_000));
        // sum of the reward commissions from both accumulators
        assert_eq!(reward_commission_b.commission_lamports, 80);

        let reward_commission_c = accumulator
            .reward_commissions
            .get(&commission_pubkey_c)
            .unwrap();
        assert_eq!(reward_commission_c.commission_bps, Some(1_000));
        assert_eq!(reward_commission_c.commission_lamports, 50);
    }

    fn check_accumulator(
        accumulator: &RewardsAccumulator,
        commission_pubkey: &Pubkey,
        left_reward: &RewardCommission,
        right_reward: &RewardCommission,
    ) {
        let reward_commission = accumulator
            .reward_commissions
            .get(commission_pubkey)
            .unwrap();

        assert_eq!(
            left_reward.is_vote_account || right_reward.is_vote_account,
            reward_commission.is_vote_account
        );
        match (left_reward.is_vote_account, right_reward.is_vote_account) {
            (false, true) => {
                assert_eq!(
                    reward_commission.commission_lamports,
                    right_reward.commission_lamports
                );
                assert_eq!(
                    reward_commission.burned_lamports,
                    left_reward.burned_lamports
                        + right_reward.burned_lamports
                        + left_reward.commission_lamports
                );
            }
            (true, false) => {
                assert_eq!(
                    reward_commission.commission_lamports,
                    left_reward.commission_lamports
                );
                assert_eq!(
                    reward_commission.burned_lamports,
                    left_reward.burned_lamports
                        + right_reward.burned_lamports
                        + right_reward.commission_lamports
                );
            }
            _ => {
                assert_eq!(
                    reward_commission.commission_lamports,
                    left_reward.commission_lamports + right_reward.commission_lamports
                );
                assert_eq!(
                    reward_commission.burned_lamports,
                    left_reward.burned_lamports + right_reward.burned_lamports
                );
            }
        }
    }

    #[test_matrix(
        [false, true],
        [false, true]
    )]
    fn test_reward_accumulator_add_rewards(
        left_is_vote_account: bool,
        right_is_vote_account: bool,
    ) {
        let mut accumulator = RewardsAccumulator::default();
        let commission_pubkey = Pubkey::new_unique();
        let commission_bps = Some(1_000);

        let left_reward = RewardCommission {
            commission_bps,
            commission_lamports: 1,
            burned_lamports: 10,
            is_vote_account: left_is_vote_account,
        };

        let right_reward = RewardCommission {
            commission_bps,
            commission_lamports: 100,
            burned_lamports: 1_000,
            is_vote_account: right_is_vote_account,
        };
        accumulator.add_reward(commission_pubkey, left_reward.clone(), 50);
        accumulator.add_reward(commission_pubkey, right_reward.clone(), 50);

        check_accumulator(
            &accumulator,
            &commission_pubkey,
            &left_reward,
            &right_reward,
        );
    }

    #[test_matrix(
        [false, true],
        [false, true],
        [false, true]
    )]
    fn test_reward_accumulator_accumulate_into_larger(
        left_is_vote_account: bool,
        right_is_vote_account: bool,
        right_is_larger: bool,
    ) {
        let mut accumulator1 = RewardsAccumulator::default();
        let mut accumulator2 = RewardsAccumulator::default();
        let commission_pubkey = Pubkey::new_unique();
        let commission_bps = Some(1_000);

        let left_reward = RewardCommission {
            commission_bps,
            commission_lamports: 1,
            burned_lamports: 10,
            is_vote_account: left_is_vote_account,
        };

        let right_reward = RewardCommission {
            commission_bps,
            commission_lamports: 100,
            burned_lamports: 1_000,
            is_vote_account: right_is_vote_account,
        };

        accumulator1.add_reward(commission_pubkey, left_reward.clone(), 50);
        accumulator2.add_reward(commission_pubkey, right_reward.clone(), 50);

        let additional_commission_pubkey = Pubkey::new_unique();
        let additional_reward_commission = RewardCommission {
            commission_bps,
            commission_lamports: 1,
            burned_lamports: 2,
            is_vote_account: true,
        };
        if right_is_larger {
            accumulator2.add_reward(
                additional_commission_pubkey,
                additional_reward_commission,
                50,
            );
        } else {
            accumulator1.add_reward(
                additional_commission_pubkey,
                additional_reward_commission,
                50,
            );
        };

        let accumulator = accumulator1.accumulate_into_larger(accumulator2);
        check_accumulator(
            &accumulator,
            &commission_pubkey,
            &left_reward,
            &right_reward,
        );
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
                    &vote_pubkey,
                    [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                    &vote_pubkey,
                    0,
                    &vote_pubkey,
                    0,
                    &vote_pubkey,
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
                    &vote_pubkey,
                    [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                    &vote_pubkey,
                    0,
                    &vote_pubkey,
                    0,
                    &vote_pubkey,
                    100_000_000_000,
                ),
                &genesis_config.rent,
                stake_lamports,
            );
            genesis_config
                .accounts
                .insert(stake_pubkey, stake_account.into());
        }

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let next_epoch_slot = bank.get_slots_in_epoch(bank.epoch());
        {
            let cache = bank.epoch_rewards_calculation_cache.lock().unwrap();
            assert!(
                !cache.contains_key(&bank.parent_hash()),
                "cache should be empty"
            );
        }

        let bank_fork1 = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank.clone(),
            SlotLeader::default(),
            next_epoch_slot,
        );
        {
            let cache = bank_fork1.epoch_rewards_calculation_cache.lock().unwrap();
            assert!(
                cache.contains_key(&bank_fork1.parent_hash()),
                "cache should be populated"
            );
        }

        // Use new_from_parent (not _with_bank_forks) - we can't insert two banks at same slot
        let bank_fork2 = Arc::new(Bank::new_from_parent(
            bank.clone(),
            SlotLeader::default(),
            next_epoch_slot,
        ));
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
                create_staked_node_accounts(stake_lamports, &bank.rent_collector.rent);
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
        expected_reward_commissions: u64,
        expected_stake_rewards: u64,
        expected_rewards: u64,
        expected_points: u128,
        parent_capitalization: Option<u64>,
    ) {
        let cache = bank.epoch_rewards_calculation_cache.lock().unwrap();
        assert_eq!(cache.len(), expected_cache_len);
        let partitioned = cache.get(&bank.parent_hash()).unwrap().as_ref();
        let reward_commissions = &partitioned.reward_commissions;
        let StakeRewardCalculation {
            stake_rewards,
            total_stake_rewards_lamports,
            ..
        } = &partitioned.stake_rewards;
        let point_value = &partitioned.point_value;
        let voters: HashSet<_> = reward_commissions.keys().copied().collect();
        let stakers: HashSet<_> = stake_rewards
            .rewards
            .iter()
            .filter_map(|reward| reward.as_ref())
            .map(|reward| reward.stake_pubkey)
            .collect();
        assert_eq!(expected_voters, &voters);
        assert_eq!(expected_stakers, &stakers);
        let total_reward_commission_lamports: u64 = reward_commissions
            .values()
            .map(|rc| rc.commission_lamports)
            .sum();
        assert_eq!(
            total_reward_commission_lamports,
            expected_reward_commissions
        );
        assert_eq!(*total_stake_rewards_lamports, expected_stake_rewards);
        assert_eq!(point_value.rewards, expected_rewards);
        assert_eq!(point_value.points, expected_points);
        if let Some(parent_cap) = parent_capitalization {
            assert_eq!(
                bank.capitalization(),
                parent_cap + expected_reward_commissions
            );
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
            0,                     // expected_reward_commissions
            499500,                // expected_stake_rewards
            499542,                // expected_rewards
            8_400_000_000_000u128, // expected_points
            None,                  // parent_capitalization
        );

        add_voters_and_populate(&bank1, &mut voters, &mut stakers, 5, 5_000_000_000, 10);
        let parent_capitalization = bank1.capitalization();

        let bank2 = Arc::new(Bank::new_from_parent(
            Arc::clone(&bank1),
            SlotLeader::default(),
            SLOTS_PER_EPOCH * 2,
        ));

        assert_cached_rewards(
            &bank2,
            2,                           // expected_cache_len
            &voters,                     // expected_voters
            &stakers,                    // expected_stakers
            5555,                        // expected_reward_commissions
            494730,                      // expected_stake_rewards
            500313,                      // expected_rewards
            9_450_000_000_000u128,       // expected_points
            Some(parent_capitalization), // parent_capitalization
        );

        add_voters_and_populate(&bank2, &mut voters, &mut stakers, 10, 8_000_000_000, 10);
        let parent_capitalization = bank2.capitalization();

        let bank3 = Arc::new(Bank::new_from_parent(
            Arc::clone(&bank2),
            SlotLeader::default(),
            SLOTS_PER_EPOCH * 3,
        ));

        assert_cached_rewards(
            &bank3,
            3,                           // expected_cache_len
            &voters,                     // expected_voters
            &stakers,                    // expected_stakers
            17300,                       // expected_reward_commissions
            485365,                      // expected_stake_rewards
            502779,                      // expected_rewards
            12_810_000_000_000u128,      // expected_points
            Some(parent_capitalization), // parent_capitalization
        );
    }

    #[test]
    fn test_load_and_reward_commission_accounts_empty() {
        let (genesis_config, _mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let reward_commissions = RewardCommissions::default();
        let result = bank.load_and_reward_commission_accounts(&reward_commissions, &thread_pool);
        assert!(result.accounts_with_rewards.is_empty());
    }

    #[test]
    fn test_load_and_reward_commission_accounts_overflow() {
        let (genesis_config, _mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let pubkey = solana_pubkey::new_rand();
        let mut commission_account = AccountSharedData::default();
        commission_account.set_lamports(u64::MAX);
        bank.store_account_and_update_capitalization(&pubkey, &commission_account);
        let mut reward_commissions = RewardCommissions::default();
        reward_commissions.insert(
            pubkey,
            RewardCommission {
                commission_bps: Some(0),
                commission_lamports: 1, // enough to overflow
                burned_lamports: 0,
                is_vote_account: true,
            },
        );
        let result = bank.load_and_reward_commission_accounts(&reward_commissions, &thread_pool);
        assert!(result.accounts_with_rewards.is_empty());
    }

    #[test]
    fn test_load_and_reward_commission_accounts_reflects_vat_burn() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let pubkey = solana_pubkey::new_rand();

        let pre_burn_balance = 10 * crate::bank::VAT_TO_BURN_PER_EPOCH;
        let commission_lamports = 12_345;

        // Commission is planned against the pre-burn account state.
        let mut commission_account = AccountSharedData::default();
        commission_account.set_lamports(pre_burn_balance);
        bank.store_account_and_update_capitalization(&pubkey, &commission_account);
        let mut reward_commissions = RewardCommissions::default();
        reward_commissions.insert(
            pubkey,
            RewardCommission {
                commission_bps: Some(500),
                commission_lamports,
                burned_lamports: 0,
                is_vote_account: true,
            },
        );

        // Simulate the VAT burn that would run in `update_epoch_stakes`
        // between reward calculation and distribution.
        let post_burn_balance = pre_burn_balance - crate::bank::VAT_TO_BURN_PER_EPOCH;
        let mut burned_account = commission_account.clone();
        burned_account.set_lamports(post_burn_balance);
        bank.store_account_and_update_capitalization(&pubkey, &burned_account);

        let result = bank.load_and_reward_commission_accounts(&reward_commissions, &thread_pool);

        assert_eq!(result.accounts_with_rewards.len(), 1);
        let (pubkey_result, reward_info, account) = &result.accounts_with_rewards[0];
        assert_eq!(*pubkey_result, pubkey);
        // Commission is credited on top of the post-burn balance, not the
        // pre-burn snapshot captured at calculation time.
        let expected_post_balance = post_burn_balance + commission_lamports;
        assert_eq!(account.lamports(), expected_post_balance);
        assert_eq!(
            *reward_info,
            RewardInfo {
                reward_type: RewardType::Voting,
                lamports: commission_lamports as i64,
                post_balance: expected_post_balance,
                commission_bps: Some(500),
            }
        );
        assert_eq!(result.amounts.distributed_lamports, commission_lamports);
    }

    #[test]
    fn test_load_and_reward_commission_accounts_normal() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let pubkey = solana_pubkey::new_rand();
        for commission_bps in [0, 100] {
            for commission_lamports in 0..2 {
                let mut commission_account = AccountSharedData::default();
                commission_account.set_lamports(1);
                bank.store_account_and_update_capitalization(&pubkey, &commission_account);
                let mut reward_commissions = RewardCommissions::default();
                reward_commissions.insert(
                    pubkey,
                    RewardCommission {
                        commission_bps: Some(commission_bps),
                        commission_lamports,
                        burned_lamports: 0,
                        is_vote_account: true,
                    },
                );
                let result =
                    bank.load_and_reward_commission_accounts(&reward_commissions, &thread_pool);
                assert_eq!(result.accounts_with_rewards.len(), 1);
                let (pubkey_result, rewards, account) = &result.accounts_with_rewards[0];
                _ = commission_account.checked_add_lamports(commission_lamports);
                assert!(accounts_equal(account, &commission_account));

                let expected_reward_info = RewardInfo {
                    reward_type: RewardType::Voting,
                    lamports: commission_lamports as i64,
                    post_balance: commission_account.lamports(),
                    commission_bps: Some(commission_bps),
                };
                assert_eq!(*rewards, expected_reward_info);
                assert_eq!(*pubkey_result, pubkey);
            }
        }
    }

    #[test]
    fn test_inflation_rewards_collector() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.rent = Rent::default();
        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let vote_address = Pubkey::new_unique();

        // Vote account just created
        let mut bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
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

        for (epoch, (collector_address, maybe_account, expect_reward)) in [
            // system account with lamports, success
            (
                Pubkey::new_unique(),
                Some(AccountSharedData::new(
                    LAMPORTS_PER_SOL,
                    0,
                    &solana_sdk_ids::system_program::id(),
                )),
                true,
            ),
            // vote account, success
            (vote_address, None, true),
            // incinerator, success
            (incinerator::id(), None, true),
            // non-rent-exempt system account with 1 lamport, success with relaxed checks
            (
                Pubkey::new_unique(),
                Some(AccountSharedData::new(
                    1,
                    0,
                    &solana_sdk_ids::system_program::id(),
                )),
                true,
            ),
            // invalid owner, no commission
            (
                Pubkey::new_unique(),
                Some(AccountSharedData::new(
                    LAMPORTS_PER_SOL,
                    0,
                    &Pubkey::new_unique(),
                )),
                false,
            ),
            // reserved account, no commission
            (solana_sdk_ids::native_loader::id(), None, false),
            // non-rent-exempt system account, no commission
            (Pubkey::new_unique(), None, false),
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(account) = maybe_account {
                bank.store_account(&collector_address, &account);
            }
            bank = apply_epoch_operations(
                bank,
                bank_forks.as_ref(),
                EpochOperations {
                    epoch: epoch as u64 + 1,
                    vote_operations: vec![(
                        vote_address,
                        VoteOperations {
                            earned_credits: Some(1),
                            new_inflation_rewards_collector: Some(collector_address),
                            expect_reward,
                            ..VoteOperations::default()
                        },
                    )],
                },
            );
        }
    }

    #[test]
    fn test_inflation_rewards_collector_becomes_rent_exempt() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.rent = Rent::default();
        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let vote_address = Pubkey::new_unique();

        // Vote account just created
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        create_with_balance: Some(LAMPORTS_PER_SOL),
                        new_commission: Some(100),
                        earned_credits: Some(1000),
                        delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                        new_inflation_rewards_collector: Some(Pubkey::new_unique()),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // next epoch, get reward into new account
        let epoch = bank.epoch();
        apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        earned_credits: Some(1),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );
    }

    #[test]
    fn test_repeated_inflation_rewards_collector() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.rent = Rent::default();
        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();

        let collector_address = Pubkey::new_unique();
        let vote1_address = Pubkey::new_unique();
        let vote2_address = Pubkey::new_unique();
        // Vote account just created
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 0,
                vote_operations: vec![
                    (
                        vote1_address,
                        VoteOperations {
                            create_with_balance: Some(LAMPORTS_PER_SOL),
                            new_commission: Some(50),
                            earned_credits: Some(1000),
                            delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                            new_inflation_rewards_collector: Some(collector_address),
                            ..VoteOperations::default()
                        },
                    ),
                    (
                        vote2_address,
                        VoteOperations {
                            create_with_balance: Some(LAMPORTS_PER_SOL),
                            new_commission: Some(100),
                            earned_credits: Some(1000),
                            delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                            new_inflation_rewards_collector: Some(collector_address),
                            ..VoteOperations::default()
                        },
                    ),
                ],
            },
        );

        // next epoch, get double reward into collector
        let epoch = bank.epoch();
        apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch,
                vote_operations: vec![
                    (
                        vote1_address,
                        VoteOperations {
                            earned_credits: Some(1),
                            expect_reward: true,
                            ..VoteOperations::default()
                        },
                    ),
                    (
                        vote2_address,
                        VoteOperations {
                            earned_credits: Some(1),
                            expect_reward: true,
                            ..VoteOperations::default()
                        },
                    ),
                ],
            },
        );
    }

    #[test]
    fn test_invalid_inflation_rewards_collector_burns_sysvar_rewards() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.rent = Rent::default();
        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let vote_address = Pubkey::new_unique();

        // Create a vote account with delegated stake and 100% commission.
        // Using 100% commission makes the expected accounting simple: every lamport
        // of the epoch reward should go through the commission collector path.
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        create_with_balance: Some(LAMPORTS_PER_SOL),
                        new_commission: Some(100),
                        earned_credits: Some(1000),
                        delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // Simulate a collector account that is invalid at reward distribution time.
        // Started as system account but now it's owned by a random program
        let invalid_collector = Pubkey::new_unique();
        bank.store_account(
            &invalid_collector,
            &AccountSharedData::new(LAMPORTS_PER_SOL, 0, &Pubkey::new_unique()),
        );

        // Point the vote account at the invalid collector and advance to the reward
        // boundary. Account is not credited, so the commission lamports are burned.
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        new_inflation_rewards_collector: Some(invalid_collector),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        let epoch_rewards = bank.get_epoch_rewards_sysvar();

        // The burned commission lamports must be reflected in the epoch rewards
        // sysvar. Since this test uses 100% commission, all calculated rewards
        // went through the invalid collector path.
        assert!(epoch_rewards.total_rewards > 0);
        assert_eq!(
            epoch_rewards.total_rewards,
            epoch_rewards.distributed_rewards
        );
    }

    #[test]
    fn test_incinerator_not_included_load_and_reward_commission_accounts() {
        let (genesis_config, _mint_keypair) = create_genesis_config(1_000 * LAMPORTS_PER_SOL);
        let bank = Bank::new_for_tests(&genesis_config);
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();

        let pubkey = incinerator::id();
        let mut incinerator_account = AccountSharedData::default();
        incinerator_account.set_lamports(1);
        bank.store_account_and_update_capitalization(&pubkey, &incinerator_account);
        let mut reward_commissions = RewardCommissions::default();

        let commission_lamports = 100;
        reward_commissions.insert(
            pubkey,
            RewardCommission {
                commission_bps: None,
                commission_lamports,
                burned_lamports: 0,
                is_vote_account: false,
            },
        );
        let result = bank.load_and_reward_commission_accounts(&reward_commissions, &thread_pool);
        assert_eq!(
            result.amounts.distributed_to_incinerator_lamports,
            commission_lamports
        );
        assert_eq!(result.amounts.distributed_lamports, 0);
        assert_eq!(result.amounts.burned_lamports, 0);
    }

    #[test]
    fn test_inflation_collector_becomes_vote_account_burns_rewards() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            42 * LAMPORTS_PER_SOL,
        );

        genesis_config.rent = Rent::default();
        genesis_config.epoch_schedule = EpochSchedule::new(SLOTS_PER_EPOCH);

        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let vote_address = Pubkey::new_unique();
        let collector_into_vote_address = Pubkey::new_unique();

        // Create a normal vote account with a currently valid inflation collector
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 0,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        create_with_balance: Some(LAMPORTS_PER_SOL),
                        new_commission: Some(100),
                        earned_credits: Some(1000),
                        delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                        new_inflation_rewards_collector: Some(collector_into_vote_address),
                        ..VoteOperations::default()
                    },
                )],
            },
        );

        // New vote account gets nothing
        let rewards = bank.get_balance(&collector_into_vote_address);
        assert_eq!(rewards, 0);

        // Next epoch gets something
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 1,
                vote_operations: vec![(
                    vote_address,
                    VoteOperations {
                        earned_credits: Some(1000),
                        expect_reward: true,
                        ..VoteOperations::default()
                    },
                )],
            },
        );
        let pre_balance = bank.get_balance(&collector_into_vote_address);
        assert_ne!(pre_balance, 0);
        // Fund the converted vote account enough to pass VAT filtering.
        let pre_balance =
            pre_balance.max(bank.get_minimum_balance_for_rent_exemption(VoteStateV4::size_of()));

        // Transform the collector into a vote account, see that all rewards
        // are burned for this epoch
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 2,
                vote_operations: vec![
                    (
                        vote_address,
                        VoteOperations {
                            earned_credits: Some(1000),
                            expect_reward: true,
                            ..VoteOperations::default()
                        },
                    ),
                    (
                        collector_into_vote_address,
                        VoteOperations {
                            create_with_balance: Some(pre_balance),
                            new_commission: Some(100),
                            earned_credits: Some(1000),
                            delegate_stake_amount: Some(LAMPORTS_PER_SOL),
                            ..VoteOperations::default()
                        },
                    ),
                ],
            },
        );

        let vote_reward = bank
            .rewards
            .read()
            .unwrap()
            .iter()
            .find(|(address, _reward)| *address == collector_into_vote_address)
            .map(|(_address, reward)| *reward)
            .unwrap();
        assert_eq!(vote_reward.lamports, 0);

        let unchanged_balance = bank.get_balance(&collector_into_vote_address);
        assert_eq!(unchanged_balance, pre_balance);

        // `collector_into_vote_address` receives its rewards, but `vote_address`
        // has its rewards burned
        let bank = apply_epoch_operations(
            bank,
            bank_forks.as_ref(),
            EpochOperations {
                epoch: 3,
                vote_operations: vec![
                    (
                        vote_address,
                        VoteOperations {
                            earned_credits: Some(1000),
                            expect_reward: true,
                            ..VoteOperations::default()
                        },
                    ),
                    (
                        collector_into_vote_address,
                        VoteOperations {
                            earned_credits: Some(1000),
                            expect_reward: true,
                            ..VoteOperations::default()
                        },
                    ),
                ],
            },
        );

        // Some rewards were distributed
        let post_balance = bank.get_balance(&collector_into_vote_address);
        assert!(post_balance > pre_balance);

        // They're reflected in the reported rewards
        let vote_reward = bank
            .rewards
            .read()
            .unwrap()
            .iter()
            .find(|(address, _reward)| *address == collector_into_vote_address)
            .map(|(_address, reward)| *reward)
            .unwrap();
        assert_eq!(vote_reward.lamports as u64, post_balance - pre_balance);

        // Some lamports were burned
        let reward_commissions = recalculate_reward_commissions_for_tests(&bank);
        let reward_commission = reward_commissions
            .get(&collector_into_vote_address)
            .unwrap();
        assert_ne!(reward_commission.burned_lamports, 0);

        // The burned lamports are included in the epoch rewards sysvar
        let epoch_rewards = bank.get_epoch_rewards_sysvar();
        assert_eq!(
            reward_commission.burned_lamports + reward_commission.commission_lamports,
            epoch_rewards.distributed_rewards
        );
    }
}
