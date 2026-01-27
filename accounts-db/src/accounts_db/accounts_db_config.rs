use {
    super::{
        AccountShrinkThreshold, DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION, MarkObsoleteAccounts,
    },
    crate::{
        accounts_file::StorageAccess,
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
    pub shrink_paths: Option<Vec<PathBuf>>,
    pub shrink_ratio: AccountShrinkThreshold,
    /// The low and high watermark sizes for the read cache, in bytes.
    /// If None, defaults will be used.
    pub read_cache_limit_bytes: Option<(usize, usize)>,
    /// The number of elements that will be randomly sampled at eviction time,
    /// the oldest of which will get evicted.
    pub read_cache_evict_sample_size: Option<usize>,
    pub write_cache_limit_bytes: Option<u64>,
    /// if None, ancient append vecs are set to ANCIENT_APPEND_VEC_DEFAULT_OFFSET
    /// Some(offset) means include slots up to (max_slot - (slots_per_epoch - 'offset'))
    pub ancient_append_vec_offset: Option<i64>,
    pub ancient_storage_ideal_size: Option<u64>,
    pub max_ancient_storages: Option<usize>,
    pub skip_initial_hash_calc: bool,
    pub exhaustively_verify_refcounts: bool,
    pub partitioned_epoch_rewards_config: PartitionedEpochRewardsConfig,
    pub storage_access: StorageAccess,
    pub scan_filter_for_shrinking: ScanFilter,
    pub mark_obsolete_accounts: MarkObsoleteAccounts,
    /// Number of threads for background operations (`thread_pool_background')
    pub num_background_threads: Option<NonZeroUsize>,
    /// Number of threads for foreground operations (`thread_pool_foreground`)
    pub num_foreground_threads: Option<NonZeroUsize>,
    /// Whether to register buffers as *fixed* in io_uring
    ///
    /// Requires memlock ulimit higher than sum of buffer sizes registered at the same time.
    pub use_registered_io_uring_buffers: bool,
}

pub const ACCOUNTS_DB_CONFIG_FOR_TESTING: AccountsDbConfig = AccountsDbConfig {
    index: Some(ACCOUNTS_INDEX_CONFIG_FOR_TESTING),
    account_indexes: None,
    bank_hash_details_dir: PathBuf::new(), // tests don't use bank hash details
    shrink_paths: None,
    shrink_ratio: DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION,
    read_cache_limit_bytes: None,
    read_cache_evict_sample_size: None,
    write_cache_limit_bytes: None,
    ancient_append_vec_offset: None,
    ancient_storage_ideal_size: None,
    max_ancient_storages: None,
    skip_initial_hash_calc: false,
    exhaustively_verify_refcounts: false,
    partitioned_epoch_rewards_config: DEFAULT_PARTITIONED_EPOCH_REWARDS_CONFIG,
    storage_access: StorageAccess::File,
    scan_filter_for_shrinking: ScanFilter::OnlyAbnormalTest,
    mark_obsolete_accounts: MarkObsoleteAccounts::Enabled,
    num_background_threads: None,
    num_foreground_threads: None,
    use_registered_io_uring_buffers: true,
};

pub const ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS: AccountsDbConfig = AccountsDbConfig {
    index: Some(ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS),
    account_indexes: None,
    bank_hash_details_dir: PathBuf::new(), // benches don't use bank hash details
    shrink_paths: None,
    shrink_ratio: DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION,
    read_cache_limit_bytes: None,
    read_cache_evict_sample_size: None,
    write_cache_limit_bytes: None,
    ancient_append_vec_offset: None,
    ancient_storage_ideal_size: None,
    max_ancient_storages: None,
    skip_initial_hash_calc: false,
    exhaustively_verify_refcounts: false,
    partitioned_epoch_rewards_config: DEFAULT_PARTITIONED_EPOCH_REWARDS_CONFIG,
    storage_access: StorageAccess::File,
    scan_filter_for_shrinking: ScanFilter::OnlyAbnormal,
    mark_obsolete_accounts: MarkObsoleteAccounts::Enabled,
    num_background_threads: None,
    num_foreground_threads: None,
    use_registered_io_uring_buffers: true,
};
