use {
    super::Bank,
    crate::{stake_account::StakeAccount, stake_history::StakeHistory},
    solana_accounts_db::stake_rewards::StakeReward,
    solana_sdk::{
        account::AccountSharedData, pubkey::Pubkey, reward_info::RewardInfo,
        stake::state::Delegation,
    },
    solana_vote::vote_account::VoteAccounts,
    std::sync::Arc,
};

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(super) enum RewardInterval {
    /// the slot within the epoch is INSIDE the reward distribution interval
    InsideInterval,
    /// the slot within the epoch is OUTSIDE the reward distribution interval
    OutsideInterval,
}

#[derive(AbiExample, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StartBlockHeightAndRewards {
    /// the block height of the slot at which rewards distribution began
    pub(crate) start_block_height: u64,
    /// calculated epoch rewards pending distribution, outer Vec is by partition (one partition per block)
    pub(crate) stake_rewards_by_partition: Arc<Vec<StakeRewards>>,
}

/// Represent whether bank is in the reward phase or not.
#[derive(AbiExample, AbiEnumVisitor, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(crate) enum EpochRewardStatus {
    /// this bank is in the reward phase.
    /// Contents are the start point for epoch reward calculation,
    /// i.e. parent_slot and parent_block height for the starting
    /// block of the current epoch.
    Active(StartBlockHeightAndRewards),
    /// this bank is outside of the rewarding phase.
    #[default]
    Inactive,
}

#[derive(Debug, Default)]
pub(super) struct VoteRewardsAccounts {
    /// reward info for each vote account pubkey.
    /// This type is used by `update_reward_history()`
    pub(super) rewards: Vec<(Pubkey, RewardInfo)>,
    /// corresponds to pubkey in `rewards`
    /// Some if account is to be stored.
    /// None if to be skipped.
    pub(super) accounts_to_store: Vec<Option<AccountSharedData>>,
}

/// hold reward calc info to avoid recalculation across functions
pub(super) struct EpochRewardCalculateParamInfo<'a> {
    pub(super) stake_history: StakeHistory,
    pub(super) stake_delegations: Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)>,
    pub(super) cached_vote_accounts: &'a VoteAccounts,
}

/// Hold all results from calculating the rewards for partitioned distribution.
/// This struct exists so we can have a function which does all the calculation with no
/// side effects.
pub(super) struct PartitionedRewardsCalculation {
    pub(super) vote_account_rewards: VoteRewardsAccounts,
    pub(super) stake_rewards_by_partition: StakeRewardCalculationPartitioned,
    pub(super) old_vote_balance_and_staked: u64,
    pub(super) validator_rewards: u64,
    pub(super) validator_rate: f64,
    pub(super) foundation_rate: f64,
    pub(super) prev_epoch_duration_in_years: f64,
    pub(super) capitalization: u64,
}

/// result of calculating the stake rewards at beginning of new epoch
pub(super) struct StakeRewardCalculationPartitioned {
    /// each individual stake account to reward, grouped by partition
    pub(super) stake_rewards_by_partition: Vec<StakeRewards>,
    /// total lamports across all `stake_rewards`
    pub(super) total_stake_rewards_lamports: u64,
}

pub(super) struct CalculateRewardsAndDistributeVoteRewardsResult {
    /// total rewards for the epoch (including both vote rewards and stake rewards)
    pub(super) total_rewards: u64,
    /// distributed vote rewards
    pub(super) distributed_rewards: u64,
    /// stake rewards that still need to be distributed, grouped by partition
    pub(super) stake_rewards_by_partition: Vec<StakeRewards>,
}

pub(crate) type StakeRewards = Vec<StakeReward>;
