//! Code related to partitioned rewards distribution

/// Baseline number of stake accounts to store in one 400ms block during the
/// partitioned reward interval.
///
/// The target is 64 rewards per entry/tick. A block has a minimum of 64
/// entries/ticks, giving 4096 total rewards to store in one 400ms block. This
/// constant affects consensus; shorter slot-time targets scale this value down
/// in `Bank` state.
pub const MAX_PARTITIONED_REWARDS_PER_BLOCK: u64 = 4096;

/// Configuration options for partitioned epoch rewards.
#[derive(Debug, Clone, Copy)]
pub struct PartitionedEpochRewardsConfig {
    /// Baseline number of stake accounts to store in one block during the
    /// partitioned reward interval.
    ///
    /// This value is stored as the 400ms-slot baseline. Runtime `Bank` state
    /// derives the effective per-bank value from the active slot-time target.
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
    /// Constructs a config with an explicit 400ms-slot baseline for tests and
    /// benchmarks.
    pub fn new_for_test(stake_account_stores_per_block: u64) -> Self {
        Self {
            stake_account_stores_per_block,
        }
    }
}
