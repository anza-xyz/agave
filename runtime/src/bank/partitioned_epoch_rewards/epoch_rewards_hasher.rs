use {
    crate::bank::partitioned_epoch_rewards::PartitionedStakeRewards,
    solana_epoch_rewards_hasher::EpochRewardsHasher, solana_hash::Hash,
};

pub(in crate::bank::partitioned_epoch_rewards) fn hash_rewards_into_partitions(
    stake_rewards: &PartitionedStakeRewards,
    parent_blockhash: &Hash,
    num_partitions: usize,
) -> Vec<Vec<usize>> {
    let hasher = EpochRewardsHasher::new(num_partitions, parent_blockhash);
    let mut indices = vec![vec![]; num_partitions];

    for (i, reward) in stake_rewards.enumerated_rewards_iter() {
        // clone here so the hasher's state is reused on each call to `hash_address_to_partition`.
        // This prevents us from re-hashing the seed each time.
        // The clone is explicit (as opposed to an implicit copy) so it is clear this is intended.
        let partition_index = hasher
            .clone()
            .hash_address_to_partition(&reward.stake_pubkey);
        indices[partition_index].push(i);
    }
    indices
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bank::{
            partitioned_epoch_rewards::{PartitionedStakeReward, REWARD_CALCULATION_NUM_BLOCKS},
            tests::create_genesis_config,
            Bank,
        },
        solana_epoch_schedule::EpochSchedule,
        solana_native_token::LAMPORTS_PER_SOL,
        std::sync::Arc,
    };

    #[test]
    fn test_hash_rewards_into_partitions() {
        // setup the expected number of stake rewards
        let expected_num = 12345;

        let stake_rewards = (0..expected_num)
            .map(|_| Some(PartitionedStakeReward::new_random()))
            .collect::<PartitionedStakeRewards>();

        let partition_indices = hash_rewards_into_partitions(&stake_rewards, &Hash::default(), 5);
        let total_num_after_hash_partition: usize = partition_indices.iter().map(|x| x.len()).sum();

        // assert total is same, so nothing is dropped or duplicated
        assert_eq!(expected_num, total_num_after_hash_partition);
    }

    #[test]
    fn test_hash_rewards_into_partitions_empty() {
        let stake_rewards = PartitionedStakeRewards::default();

        let num_partitions = 5;
        let partition_indices =
            hash_rewards_into_partitions(&stake_rewards, &Hash::default(), num_partitions);

        assert_eq!(partition_indices.len(), num_partitions);
        for indices in partition_indices.iter().take(num_partitions) {
            assert!(indices.is_empty());
        }
    }

    /// Test that reward partition range panics when passing out of range partition index
    #[test]
    #[should_panic(expected = "index out of bounds: the len is 10 but the index is 15")]
    fn test_get_stake_rewards_partition_range_panic() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        genesis_config.epoch_schedule = EpochSchedule::custom(432000, 432000, false);
        let mut bank = Bank::new_for_tests(&genesis_config);

        // simulate 40K - 1 rewards, the expected num of credit blocks should be 10.
        let expected_num = 40959;
        let stake_rewards = (0..expected_num)
            .map(|_| Some(PartitionedStakeReward::new_random()))
            .collect::<PartitionedStakeRewards>();

        let partition_indices =
            hash_rewards_into_partitions(&stake_rewards, &Hash::new_from_array([1; 32]), 10);

        bank.set_epoch_reward_status_distribution(
            bank.block_height() + REWARD_CALCULATION_NUM_BLOCKS,
            Arc::new(stake_rewards),
            partition_indices.clone(),
        );

        // This call should panic, i.e. 15 is out of the num_credit_blocks
        let _range = &partition_indices[15];
    }
}
