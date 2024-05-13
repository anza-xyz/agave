use {
    super::Bank,
    solana_sdk::{reward_info::RewardInfo, reward_type::RewardType, sysvar},
};

impl Bank {
    pub(in crate::bank::partitioned_epoch_rewards) fn record_partition_data_reward(
        &self,
        num_partitions: usize,
    ) {
        let mut rewards = self.rewards.write().unwrap();
        rewards.reserve(1);
        rewards.push((
            // rewards must be "delivered" to some address; since this
            // RewardType is for records purposes only, the address is arbitrary
            sysvar::epoch_rewards::id(),
            RewardInfo {
                reward_type: RewardType::PartitionData,
                lamports: 0,
                post_balance: 0,
                commission: None,
                num_partitions: Some(num_partitions),
            },
        ));
    }
}
