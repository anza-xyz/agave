use {
    super::{AccountShrinkThreshold, DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION},
    crate::{
        accounts_file::AccountsFileProvider,
        accounts_index::{
            ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS, ACCOUNTS_INDEX_CONFIG_FOR_TESTING,
            AccountSecondaryIndexes, AccountsIndexConfig, ScanFilter,
        },
        partitioned_rewards::{
            DEFAULT_PARTITIONED_EPOCH_REWARDS_CONFIG, PartitionedEpochRewardsConfig,
        },
    },
    std::{num::NonZeroUsize, path::PathBuf},
};

#[derive(Debug, Default, Clone)]
pub struct AccountsDbConfig {
    pub index: Option<AccountsIndexConfig>,
    pub account_indexes: Option<AccountSecondaryIndexes>,
    pub bank_hash_details_dir: PathBuf,
    pub shrink_ratio: AccountShrinkThreshold,
    /// The low and high watermark sizes for the read cache, in bytes.
    /// If None, defaults will be used.
    pub read_cache_limit_bytes: Option<(usize, usize)>,
    /// The number of elements that will be randomly sampled at eviction time,
    /// the oldest of which will get evicted.
    pub read_cache_evict_sample_size: Option<usize>,
    /// Number of shards for the read-only accounts cache's DashMap.
    /// Must be a power of two. If None, defaults to 65536.
    pub read_cache_num_shards: Option<usize>,
    pub write_cache_limit_bytes: Option<u64>,
    /// if None, ancient append vecs are set to ANCIENT_APPEND_VEC_DEFAULT_OFFSET
    /// Some(offset) means include slots up to (max_slot - (slots_per_epoch - 'offset'))
    pub ancient_append_vec_offset: Option<i64>,
    pub ancient_storage_ideal_size: Option<u64>,
    pub max_ancient_storages: Option<usize>,
    pub skip_initial_hash_calc: bool,
    pub exhaustively_verify_refcounts: bool,
    pub partitioned_epoch_rewards_config: PartitionedEpochRewardsConfig,
    pub scan_filter_for_shrinking: ScanFilter,
    /// Number of threads for background operations (`thread_pool_background')
    pub num_background_threads: Option<NonZeroUsize>,
    /// Number of threads for foreground operations (`thread_pool_foreground`)
    pub num_foreground_threads: Option<NonZeroUsize>,
    pub accounts_file_provider: AccountsFileProvider,
}

#[cfg(feature = "dev-context-only-utils")]
impl AccountsDbConfig {
    /// AccountsDb configuration optimized for short-lived, single-threaded test invocations.
    pub fn new_for_tests_single_threaded() -> Self {
        let single_thread = NonZeroUsize::new(1).unwrap();
        Self {
            index: Some(AccountsIndexConfig {
                bins: Some(2),
                num_flush_threads: Some(single_thread),
                index_limit: crate::accounts_index::IndexLimit::InMemOnly,
                ..AccountsIndexConfig::default()
            }),
            skip_initial_hash_calc: true,
            num_background_threads: Some(single_thread),
            num_foreground_threads: Some(single_thread),
            exhaustively_verify_refcounts: false,
            read_cache_num_shards: Some(2),
            ..Self::default()
        }
    }
}

pub const ACCOUNTS_DB_CONFIG_FOR_TESTING: AccountsDbConfig = AccountsDbConfig {
    index: Some(ACCOUNTS_INDEX_CONFIG_FOR_TESTING),
    account_indexes: None,
    bank_hash_details_dir: PathBuf::new(), // tests don't use bank hash details
    shrink_ratio: DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION,
    read_cache_limit_bytes: None,
    read_cache_evict_sample_size: None,
    read_cache_num_shards: None,
    write_cache_limit_bytes: None,
    ancient_append_vec_offset: None,
    ancient_storage_ideal_size: None,
    max_ancient_storages: None,
    skip_initial_hash_calc: false,
    exhaustively_verify_refcounts: false,
    partitioned_epoch_rewards_config: DEFAULT_PARTITIONED_EPOCH_REWARDS_CONFIG,
    scan_filter_for_shrinking: ScanFilter::OnlyAbnormalTest,
    num_background_threads: None,
    num_foreground_threads: None,
    accounts_file_provider: AccountsFileProvider::AppendVec,
};

pub const ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS: AccountsDbConfig = AccountsDbConfig {
    index: Some(ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS),
    account_indexes: None,
    bank_hash_details_dir: PathBuf::new(), // benches don't use bank hash details
    shrink_ratio: DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION,
    read_cache_limit_bytes: None,
    read_cache_evict_sample_size: None,
    read_cache_num_shards: None,
    write_cache_limit_bytes: None,
    ancient_append_vec_offset: None,
    ancient_storage_ideal_size: None,
    max_ancient_storages: None,
    skip_initial_hash_calc: false,
    exhaustively_verify_refcounts: false,
    partitioned_epoch_rewards_config: DEFAULT_PARTITIONED_EPOCH_REWARDS_CONFIG,
    scan_filter_for_shrinking: ScanFilter::OnlyAbnormal,
    num_background_threads: None,
    num_foreground_threads: None,
    accounts_file_provider: AccountsFileProvider::AppendVec,
};

#[cfg(all(test, feature = "dev-context-only-utils"))]
mod tests {
    use {super::AccountsDbConfig, crate::accounts_index::IndexLimit};

    #[test]
    fn test_new_for_tests_single_threaded_config() {
        let config = AccountsDbConfig::new_for_tests_single_threaded();
        let index = config.index.unwrap();

        assert_eq!(index.bins, Some(2));
        assert_eq!(index.num_flush_threads.map(usize::from), Some(1));
        assert!(matches!(index.index_limit, IndexLimit::InMemOnly));
        assert!(config.skip_initial_hash_calc);
        assert!(!config.exhaustively_verify_refcounts);
        assert_eq!(config.read_cache_num_shards, Some(2));
        assert_eq!(config.num_background_threads.map(usize::from), Some(1));
        assert_eq!(config.num_foreground_threads.map(usize::from), Some(1));
    }
}
