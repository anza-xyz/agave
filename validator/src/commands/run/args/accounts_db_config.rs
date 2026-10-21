use {
    crate::{
        cli::{DefaultArgs, thread_args::NumThreadConfig},
        commands::FromClapArgMatches,
    },
    bytesize::ByteSize,
    clap::{Arg, ArgMatches, value_t, values_t},
    log::warn,
    solana_accounts_db::{
        accounts_db::{AccountShrinkThreshold, AccountsDbConfig},
        accounts_file::AccountsFileProvider,
        accounts_index::{
            AccountSecondaryIndexes, AccountsIndexConfig, DEFAULT_NUM_ENTRIES_OVERHEAD,
            DEFAULT_NUM_ENTRIES_TO_EVICT, IndexLimit, IndexLimitThreshold,
            MINIMAL_THRESHOLD_NUM_BYTES, ScanFilter,
        },
        partitioned_rewards::PartitionedEpochRewardsConfig,
    },
    solana_clap_utils::{
        hidden_unless_forced,
        input_parsers::{value_of, values_of},
        input_validators::{is_parsable, is_pow2},
    },
    std::{
        error::Error,
        path::{Path, PathBuf},
    },
};

pub(crate) fn new_config(
    matches: &ArgMatches,
    ledger_path: &Path,
    num_threads: &NumThreadConfig,
    use_direct_io: bool,
) -> Result<AccountsDbConfig, Box<dyn Error>> {
    let account_indexes = AccountSecondaryIndexes::from_clap_arg_match(matches)?;

    let accounts_shrink_optimize_total_space =
        value_t!(matches, "accounts_shrink_optimize_total_space", bool)?;

    let shrink_ratio = value_t!(matches, "accounts_shrink_ratio", f64)?;
    if !(0.0..=1.0).contains(&shrink_ratio) {
        Err(format!(
            "the specified account-shrink-ratio is invalid, it must be between 0. and 1.0 \
             inclusive: {shrink_ratio}"
        ))?;
    }

    let shrink_ratio = if accounts_shrink_optimize_total_space {
        AccountShrinkThreshold::TotalSpace { shrink_ratio }
    } else {
        AccountShrinkThreshold::IndividualStore { shrink_ratio }
    };

    let accounts_index_limit = value_t!(matches, "accounts_index_limit", String)?;
    enum CliIndexLimit {
        Unlimited,
        Threshold(u64),
    }
    let cli_index_limit = match accounts_index_limit.as_str() {
        "minimal" => {
            warn!(
                "Using `minimal` for `--accounts-index-limit` is deprecated. Using 25GB instead."
            );
            CliIndexLimit::Threshold(MINIMAL_THRESHOLD_NUM_BYTES)
        }
        "unlimited" => CliIndexLimit::Unlimited,
        "25GB" => CliIndexLimit::Threshold(25_000_000_000),
        "50GB" => CliIndexLimit::Threshold(50_000_000_000),
        "100GB" => CliIndexLimit::Threshold(100_000_000_000),
        "200GB" => CliIndexLimit::Threshold(200_000_000_000),
        "400GB" => CliIndexLimit::Threshold(400_000_000_000),
        "800GB" => CliIndexLimit::Threshold(800_000_000_000),
        x => {
            // clap will enforce only the above values are possible
            unreachable!("invalid value given to `--accounts-index-limit`: '{x}'")
        }
    };

    // Note: need to still handle --enable-accounts-disk-index until it is removed
    let cli_index_limit = if matches.is_present("enable_accounts_disk_index") {
        CliIndexLimit::Threshold(MINIMAL_THRESHOLD_NUM_BYTES)
    } else {
        cli_index_limit
    };

    let index_limit = match cli_index_limit {
        CliIndexLimit::Unlimited => IndexLimit::InMemOnly,
        CliIndexLimit::Threshold(num_bytes) => IndexLimit::Threshold(IndexLimitThreshold {
            num_bytes,
            num_entries_overhead: DEFAULT_NUM_ENTRIES_OVERHEAD,
            num_entries_to_evict: DEFAULT_NUM_ENTRIES_TO_EVICT,
        }),
    };

    let mut accounts_index_config = AccountsIndexConfig {
        num_flush_threads: Some(num_threads.accounts_index_flush_threads),
        index_limit,
        ..AccountsIndexConfig::default()
    };
    if let Ok(bins) = value_t!(matches, "accounts_index_bins", usize) {
        accounts_index_config.bins = Some(bins);
    }
    if let Ok(num_initial_accounts) =
        value_t!(matches, "accounts_index_initial_accounts_count", usize)
    {
        accounts_index_config.num_initial_accounts = Some(num_initial_accounts);
    }

    {
        let mut accounts_index_paths: Vec<PathBuf> = if matches.is_present("accounts_index_path") {
            values_t!(matches, "accounts_index_path", String)?
                .into_iter()
                .map(PathBuf::from)
                .collect()
        } else {
            vec![]
        };
        if accounts_index_paths.is_empty() {
            accounts_index_paths = vec![ledger_path.join("accounts_index")];
        }
        accounts_index_config.drives = Some(accounts_index_paths);
    }

    const MB: usize = 1_024 * 1_024;

    let read_cache_limit_bytes = if let Some(limits) =
        values_of::<ByteSize>(matches, "accounts_db_read_cache_limit")
    {
        match limits.as_slice() {
            [lo, hi] => {
                let lo = usize::try_from(lo.0)?;
                let hi = usize::try_from(hi.0)?;
                if lo > hi {
                    Err(format!(
                        "invalid --accounts-db-read-cache-limit: LOW ({lo}) must be <= HIGH ({hi})",
                    ))?;
                }
                Some((lo, hi))
            }
            _ => {
                // clap will enforce two values are given
                unreachable!("invalid number of values given to accounts-db-read-cache-limit")
            }
        }
    } else {
        None
    };

    let write_cache_limit_bytes =
        value_of::<ByteSize>(matches, "accounts_db_write_cache_limit").map(|limit| limit.0);
    // --accounts-db-cache-limit-mb was deprecated in v4.2.0
    let write_cache_limit_mb = value_t!(matches, "accounts_db_cache_limit_mb", u64)
        .ok()
        .map(|mb| mb * MB as u64);
    // clap will enforce only one cli arg is provided, so pick whichever is Some
    let write_cache_limit_bytes = write_cache_limit_bytes.or(write_cache_limit_mb);

    let scan_filter_for_shrinking = matches
        .value_of("accounts_db_scan_filter_for_shrinking")
        .map(|filter| match filter {
            "all" => ScanFilter::All,
            "only-abnormal" => ScanFilter::OnlyAbnormal,
            "only-abnormal-with-verify" => ScanFilter::OnlyAbnormalWithVerify,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts_db_scan_filter_for_shrinking")
            }
        })
        .unwrap_or_default();

    let accounts_file_provider = matches
        .value_of("accounts_db_account_storage_file_format")
        .map(|format| match format {
            "append-vec" => AccountsFileProvider::AppendVec,
            "split-experimental" => AccountsFileProvider::Split,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts_db_account_storage_file_format")
            }
        })
        .expect("--accounts-db-account-storage-file-format has a default value");

    Ok(AccountsDbConfig {
        index: Some(accounts_index_config),
        account_indexes: Some(account_indexes),
        bank_hash_details_dir: ledger_path.to_path_buf(),
        shrink_ratio,
        read_cache_limit_bytes,
        read_cache_evict_sample_size: None,
        read_cache_num_shards: None,
        write_cache_limit_bytes,
        ancient_append_vec_offset: value_t!(matches, "accounts_db_ancient_append_vecs", i64).ok(),
        ancient_storage_ideal_size: value_t!(
            matches,
            "accounts_db_ancient_storage_ideal_size",
            u64
        )
        .ok(),
        max_ancient_storages: value_t!(matches, "accounts_db_max_ancient_storages", usize).ok(),
        skip_initial_hash_calc: false,
        verify_index: matches.is_present("accounts_db_verify_index"),
        partitioned_epoch_rewards_config: PartitionedEpochRewardsConfig::default(),
        scan_filter_for_shrinking,
        num_background_threads: Some(num_threads.accounts_db_background_threads),
        accounts_file_provider,
        use_direct_io,
    })
}

pub(crate) fn args(default_args: &DefaultArgs) -> Vec<Arg<'_, '_>> {
    vec![
        Arg::with_name("accounts_db_verify_index")
            .long("accounts-db-verify-index")
            .help(
                "Debug option to scan all storages and verify account index slot lists prior to \
                 clean",
            )
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_db_scan_filter_for_shrinking")
            .long("accounts-db-scan-filter-for-shrinking")
            .takes_value(true)
            .possible_values(&["all", "only-abnormal", "only-abnormal-with-verify"])
            .help(
                "Debug option to use different type of filtering for accounts index scan in \
                 shrinking. \"all\" will scan both in-memory and on-disk accounts index, which is \
                 the default. \"only-abnormal\" will scan in-memory accounts index only for \
                 abnormal entries and skip scanning on-disk accounts index by assuming that \
                 on-disk accounts index contains only normal accounts index entry. \
                 \"only-abnormal-with-verify\" is similar to \"only-abnormal\", which will scan \
                 in-memory index for abnormal entries, but will also verify that on-disk account \
                 entries are indeed normal.",
            )
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_db_account_storage_file_format")
            .long("accounts-db-account-storage-file-format")
            .takes_value(true)
            .possible_values(&["append-vec", "split-experimental"])
            .default_value("append-vec")
            .help("Selects the account storage file format")
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_db_ancient_append_vecs")
            .long("accounts-db-ancient-append-vecs")
            .value_name("SLOT-OFFSET")
            .validator(is_parsable::<i64>)
            .takes_value(true)
            .help(
                "AppendVecs that are older than (slots_per_epoch - SLOT-OFFSET) are squashed \
                 together.",
            )
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_db_ancient_storage_ideal_size")
            .long("accounts-db-ancient-storage-ideal-size")
            .value_name("BYTES")
            .validator(is_parsable::<u64>)
            .takes_value(true)
            .help("The smallest size of ideal ancient storage.")
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_db_max_ancient_storages")
            .long("accounts-db-max-ancient-storages")
            .value_name("USIZE")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help("The number of ancient storages the ancient slot combining should converge to.")
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_db_write_cache_limit")
            .long("accounts-db-write-cache-limit")
            .value_name("BYTES")
            .validator(is_parsable::<ByteSize>)
            .takes_value(true)
            .help("How large the write cache for account data can become, in bytes")
            .long_help(
                "How large the write cache for account data can become, in bytes. If this is \
                 exceeded, the write cache is flushed more aggressively. Accepts SI and IEC \
                 prefixes, e.g. 17.1GB or 18Gi.",
            ),
        Arg::with_name("accounts_db_read_cache_limit")
            .long("accounts-db-read-cache-limit")
            .value_name("LOW,HIGH")
            .takes_value(true)
            .min_values(2)
            .max_values(2)
            .multiple(false)
            .require_delimiter(true)
            .validator(is_parsable::<ByteSize>)
            .help("How large the read cache for account data can become, in bytes")
            .long_help(
                "How large the read cache for account data can become, in bytes. The values will \
                 be the low and high watermarks for the cache. When the cache exceeds the high \
                 watermark, entries will be evicted until the size reaches the low watermark. \
                 Accepts SI and IEC prefixes, e.g. 8.1GB,8.3GiB. LOW must be <= HIGH.",
            )
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_index_bins")
            .long("accounts-index-bins")
            .value_name("BINS")
            .validator(is_pow2)
            .takes_value(true)
            .help("Number of bins to divide the accounts index into"),
        Arg::with_name("accounts_index_limit")
            .long("accounts-index-limit")
            .value_name("VALUE")
            .takes_value(true)
            .possible_values(&[
                "minimal",
                "25GB",
                "50GB",
                "100GB",
                "200GB",
                "400GB",
                "800GB",
                "unlimited",
            ])
            .default_value("unlimited")
            .help("Sets the memory limit for the accounts index")
            .long_help(
                "Sets the memory limit for the accounts index. The size options will limit the \
                 accounts index memory to the specified value. E.g. \"50GB\" means the accounts \
                 index may use up to 50 GB of memory. The \"unlimited\" option keeps the entire \
                 accounts index in memory. All index entries that are not in memory are kept in \
                 the disk-backed index. The disk-backed index has lower performance; prefer \
                 higher explicit limits here. \"minimal\" is deprecated and behaves as \"25GB\".",
            ),
        Arg::with_name("accounts_index_initial_accounts_count")
            .long("accounts-index-initial-accounts-count")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help("Pre-allocate the accounts index, assuming this many accounts")
            .long_help(
                "Pre-allocate the accounts index, assuming this many accounts. Overrides the \
                 account count recorded in the local snapshot directory, which is otherwise used \
                 when starting from local snapshot state rather than a snapshot archive.",
            )
            .hidden(hidden_unless_forced()),
        Arg::with_name("accounts_index_path")
            .long("accounts-index-path")
            .value_name("PATH")
            .takes_value(true)
            .multiple(true)
            .help(
                "Persistent accounts-index location. May be specified multiple times. [default: \
                 <LEDGER>/accounts_index]",
            ),
        Arg::with_name("accounts_shrink_optimize_total_space")
            .long("accounts-shrink-optimize-total-space")
            .takes_value(true)
            .value_name("BOOLEAN")
            .default_value(&default_args.accounts_shrink_optimize_total_space)
            .validator(is_parsable::<bool>)
            .help(
                "When this is set to true, the system will shrink the most sparse accounts and \
                 when the overall shrink ratio is above the specified accounts-shrink-ratio, the \
                 shrink will stop and it will skip all other less sparse accounts.",
            ),
        Arg::with_name("accounts_shrink_ratio")
            .long("accounts-shrink-ratio")
            .takes_value(true)
            .value_name("RATIO")
            .default_value(&default_args.accounts_shrink_ratio)
            .validator(is_parsable::<f64>)
            .help(
                "Specifies the shrink ratio for the accounts to be shrunk. The shrink ratio is \
                 defined as the ratio of the bytes alive over the  total bytes used. If the \
                 account's shrink ratio is less than this ratio it becomes a candidate for \
                 shrinking. The value must between 0. and 1.0 inclusive.",
            ),
    ]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::cli::{app, thread_args::parse_num_threads_args},
    };

    fn new_test_config(args: &[&str]) -> Result<AccountsDbConfig, Box<dyn Error>> {
        let default_args = DefaultArgs::default();
        let matches =
            app("test", &default_args).get_matches_from(["agave-validator"].iter().chain(args));
        new_config(
            &matches,
            Path::new("ledger"),
            &parse_num_threads_args(&matches),
            true,
        )
    }

    #[test]
    fn test_new_config_defaults() {
        let config = new_test_config(&[]).unwrap();
        assert_eq!(
            config.index.unwrap().drives,
            Some(vec![PathBuf::from("ledger/accounts_index")])
        );
        assert_eq!(config.read_cache_limit_bytes, None);
        assert_eq!(config.write_cache_limit_bytes, None);
        assert!(config.use_direct_io);
    }

    #[test]
    fn test_index_limit() {
        let index_limit = |args: &[&str]| new_test_config(args).unwrap().index.unwrap().index_limit;
        assert!(matches!(
            index_limit(&["--accounts-index-limit", "unlimited"]),
            IndexLimit::InMemOnly
        ));
        assert!(matches!(
            index_limit(&["--accounts-index-limit", "50GB"]),
            IndexLimit::Threshold(IndexLimitThreshold {
                num_bytes: 50_000_000_000,
                ..
            })
        ));
        assert!(matches!(
            index_limit(&["--enable-accounts-disk-index"]),
            IndexLimit::Threshold(IndexLimitThreshold {
                num_bytes: MINIMAL_THRESHOLD_NUM_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn test_cache_limits() {
        let config = new_test_config(&["--accounts-db-read-cache-limit", "1KiB,2KiB"]).unwrap();
        assert_eq!(config.read_cache_limit_bytes, Some((1024, 2048)));
        let error = new_test_config(&["--accounts-db-read-cache-limit", "2KiB,1KiB"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("LOW (2048) must be <= HIGH (1024)")
        );

        let write_cache_limit_bytes =
            |args: &[&str]| new_test_config(args).unwrap().write_cache_limit_bytes;
        assert_eq!(
            write_cache_limit_bytes(&["--accounts-db-write-cache-limit", "1KiB"]),
            Some(1024)
        );
        assert_eq!(
            write_cache_limit_bytes(&["--accounts-db-cache-limit-mb", "2"]),
            Some(2 * 1024 * 1024)
        );
    }

    #[test]
    fn test_shrink_ratio() {
        let config = new_test_config(&[
            "--accounts-shrink-ratio",
            "0.5",
            "--accounts-shrink-optimize-total-space",
            "false",
        ])
        .unwrap();
        assert!(matches!(
            config.shrink_ratio,
            AccountShrinkThreshold::IndividualStore { shrink_ratio } if shrink_ratio == 0.5
        ));
        let error = new_test_config(&["--accounts-shrink-ratio", "2.0"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("account-shrink-ratio is invalid")
        );
    }
}
