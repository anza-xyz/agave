use {
    super::{Bank, EpochRewardStatus},
    crate::bank::metrics::{report_partitioned_reward_metrics, RewardsStoreMetrics},
    solana_accounts_db::stake_rewards::StakeReward,
    solana_measure::measure_us,
    solana_sdk::account::ReadableAccount,
    std::sync::atomic::Ordering::Relaxed,
};

impl Bank {
    /// Process reward distribution for the block if it is inside reward interval.
    pub(in crate::bank) fn distribute_partitioned_epoch_rewards(&mut self) {
        let EpochRewardStatus::Active(status) = &self.epoch_reward_status else {
            return;
        };

        let height = self.block_height();
        let start_block_height = status.start_block_height;
        let credit_start = start_block_height + self.get_reward_calculation_num_blocks();
        let credit_end_exclusive = credit_start + status.stake_rewards_by_partition.len() as u64;
        assert!(
            self.epoch_schedule.get_slots_in_epoch(self.epoch)
                > credit_end_exclusive.saturating_sub(credit_start)
        );

        if height >= credit_start && height < credit_end_exclusive {
            let partition_index = height - credit_start;
            self.distribute_epoch_rewards_in_partition(
                &status.stake_rewards_by_partition,
                partition_index,
            );
        }

        if height.saturating_add(1) >= credit_end_exclusive {
            datapoint_info!(
                "epoch-rewards-status-update",
                ("slot", self.slot(), i64),
                ("block_height", height, i64),
                ("active", 0, i64),
                ("start_block_height", start_block_height, i64),
            );

            assert!(matches!(
                self.epoch_reward_status,
                EpochRewardStatus::Active(_)
            ));
            self.epoch_reward_status = EpochRewardStatus::Inactive;
            self.destroy_epoch_rewards_sysvar();
        }
    }

    /// Process reward credits for a partition of rewards
    /// Store the rewards to AccountsDB, update reward history record and total capitalization.
    pub(in crate::bank) fn distribute_epoch_rewards_in_partition(
        &self,
        all_stake_rewards: &[Vec<StakeReward>],
        partition_index: u64,
    ) {
        let pre_capitalization = self.capitalization();
        let this_partition_stake_rewards = &all_stake_rewards[partition_index as usize];

        let (total_rewards_in_lamports, store_stake_accounts_us) =
            measure_us!(self.store_stake_accounts_in_partition(this_partition_stake_rewards));

        // increase total capitalization by the distributed rewards
        self.capitalization
            .fetch_add(total_rewards_in_lamports, Relaxed);

        // decrease distributed capital from epoch rewards sysvar
        self.update_epoch_rewards_sysvar(total_rewards_in_lamports);

        // update reward history for this partitioned distribution
        self.update_reward_history_in_partition(this_partition_stake_rewards);

        let metrics = RewardsStoreMetrics {
            pre_capitalization,
            post_capitalization: self.capitalization(),
            total_stake_accounts_count: all_stake_rewards.len(),
            partition_index,
            store_stake_accounts_us,
            store_stake_accounts_count: this_partition_stake_rewards.len(),
            distributed_rewards: total_rewards_in_lamports,
        };

        report_partitioned_reward_metrics(self, metrics);
    }

    /// insert non-zero stake rewards to self.rewards
    /// Return the number of rewards inserted
    pub(in crate::bank) fn update_reward_history_in_partition(
        &self,
        stake_rewards: &[StakeReward],
    ) -> usize {
        let mut rewards = self.rewards.write().unwrap();
        rewards.reserve(stake_rewards.len());
        let initial_len = rewards.len();
        stake_rewards
            .iter()
            .filter(|x| x.get_stake_reward() > 0)
            .for_each(|x| rewards.push((x.stake_pubkey, x.stake_reward_info)));
        rewards.len().saturating_sub(initial_len)
    }

    /// store stake rewards in partition
    /// return the sum of all the stored rewards
    ///
    /// Note: even if staker's reward is 0, the stake account still needs to be stored because
    /// credits observed has changed
    pub(in crate::bank) fn store_stake_accounts_in_partition(
        &self,
        stake_rewards: &[StakeReward],
    ) -> u64 {
        // Verify that stake account `lamports + reward_amount` matches what we have in the
        // rewarded account. This code will have a performance hit - an extra load and compare of
        // the stake accounts. This is for debugging. Once we are confident, we can disable the
        // check.
        const VERIFY_REWARD_LAMPORT: bool = true;

        if VERIFY_REWARD_LAMPORT {
            for r in stake_rewards {
                let stake_pubkey = r.stake_pubkey;
                let reward_amount = r.get_stake_reward();
                let post_stake_account = &r.stake_account;
                if let Some(curr_stake_account) = self.get_account_with_fixed_root(&stake_pubkey) {
                    let pre_lamport = curr_stake_account.lamports();
                    let post_lamport = post_stake_account.lamports();
                    assert_eq!(pre_lamport + u64::try_from(reward_amount).unwrap(), post_lamport,
                               "stake account balance has changed since the reward calculation! account: {stake_pubkey}, pre balance: {pre_lamport}, post balance: {post_lamport}, rewards: {reward_amount}");
                }
            }
        }

        self.store_accounts((self.slot(), stake_rewards));
        stake_rewards
            .iter()
            .map(|stake_reward| stake_reward.stake_reward_info.lamports)
            .sum::<i64>() as u64
    }
}
