//! Code related to partitioned rewards distribution

/// # stake accounts to store in one block during partitioned reward interval
/// Target to store 64 rewards per entry/tick in a block. A block has a minimum of 64
/// entries/tick. This gives 4096 total rewards to store in one block.
/// This constant affects consensus.
const MAX_PARTITIONED_REWARDS_PER_BLOCK: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
/// Configuration options for partitioned epoch rewards.
pub struct PartitionedEpochRewardsConfig {
    /// number of stake accounts to store in one block during partitioned reward interval
    /// normally, this is a number tuned for reasonable performance, such as 4096 accounts/block
    pub stake_account_stores_per_block: u64,
}

/// Convenient constant for default partitioned epoch rewards configuration
/// used for benchmarks and tests.
pub const DEFAULT_PARTITIONED_EPOCH_REWARDS_CONFIG: PartitionedEpochRewardsConfig =
    PartitionedEpochRewardsConfig {
        stake_account_stores_per_block: MAX_PARTITIONED_REWARDS_PER_BLOCK,
    };

impl Default for PartitionedEpochRewardsConfig {
    fn default() -> Self {
        Self {
            stake_account_stores_per_block: MAX_PARTITIONED_REWARDS_PER_BLOCK,
        }
    }
}

impl PartitionedEpochRewardsConfig {
    /// Only for tests and benchmarks
    pub fn new_for_test(stake_account_stores_per_block: u64) -> Self {
        Self {
            stake_account_stores_per_block,
        }
    }
}
