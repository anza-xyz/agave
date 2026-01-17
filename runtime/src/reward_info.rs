//! Reward info.

use {solana_accounts_db::stake_rewards::StakeRewardInfo, solana_reward_info::RewardType};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RewardInfo {
    pub reward_type: RewardType,
    pub lamports: i64,
    pub post_balance: u64,
    pub commission_bps: Option<u16>,
}

impl From<StakeRewardInfo> for RewardInfo {
    fn from(value: StakeRewardInfo) -> Self {
        Self {
            reward_type: value.reward_type,
            lamports: value.lamports,
            post_balance: value.post_balance,
            commission_bps: value.commission_bps,
        }
    }
}
