use {
    crate::LEDGER_TOOL_DIRECTORY,
    clap::{value_t, value_t_or_exit, values_t, values_t_or_exit, Arg, ArgMatches},
    log::*,
    solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig},
    solana_accounts_db::{
        accounts_db::{AccountsDbConfig, DEFAULT_MEMLOCK_BUDGET_SIZE},
        accounts_file::StorageAccess,
        accounts_index::{AccountsIndexConfig, IndexLimit, ScanFilter},
    },
    solana_clap_utils::{
        hidden_unless_forced,
        input_parsers::pubkeys_of,
        input_validators::{is_parsable, is_pow2},
    },
    solana_cli_output::CliAccountNewConfig,
    solana_clock::Slot,
    solana_ledger::{
        blockstore_processor::ProcessOptions,
        use_snapshot_archives_at_startup::{self, UseSnapshotArchivesAtStartup},
    },
    solana_runtime::runtime_config::RuntimeConfig,
    std::{
        collections::HashSet,
        path::{Path, PathBuf},
        sync::Arc,
    },
};

/// Returns the arguments that configure AccountsDb
pub fn accounts_db_args<'a, 'b>() -> Box<[Arg<'a, 'b>]> {
    vec![
        Arg::with_name("account_paths")
            .long("accounts")
            .value_name("PATHS")
            .takes_value(true)
            .help(
                "Persistent accounts location. May be specified multiple times. [default: \
                 <LEDGER>/accounts]",
            ),
        Arg::with_name("accounts_index_path")
            .long("accounts-index-path")
            .value_name("PATH")
            .takes_value(true)
            .multiple(true)
            .requires("enable_accounts_disk_index")
            .help(
                "Persistent accounts-index location. May be specified multiple times. [default: \
                 <LEDGER>/accounts_index]",
            ),
        Arg::with_name("accounts_index_bins")
            .long("accounts-index-bins")
            .value_name("BINS")
            .validator(is_pow2)
            .takes_value(true)
            .help("Number of bins to divide the accounts index into"),
        Arg::with_name("accounts_index_initial_accounts_count")
            .long("accounts-index-initial-accounts-count")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help("Pre-allocate the accounts index, assuming this many accounts")
            .hidden(hidden_unless_forced()),
        Arg::with_name("enable_accounts_disk_index")
            .long("enable-accounts-disk-index")
            .help("Enables the disk-based accounts index")
            .long_help(
                "Enables the disk-based accounts index. Reduce the memory footprint of the \
                 accounts index at the cost of index performance.",
            ),
        Arg::with_name("accounts_db_skip_shrink")
            .long("accounts-db-skip-shrink")
            .help(
                "Enables faster starting of ledger-tool by skipping shrink. This option is for \
                 use during testing.",
            ),
        Arg::with_name("accounts_db_verify_refcounts")
            .long("accounts-db-verify-refcounts")
            .help(
                "Debug option to scan all AppendVecs and verify account index refcounts prior to \
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
        Arg::with_name("accounts_db_skip_initial_hash_calculation")
            .long("accounts-db-skip-initial-hash-calculation")
            .help("Do not verify accounts hash at startup.")
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
        Arg::with_name("accounts_db_access_storages_method")
            .long("accounts-db-access-storages-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(&["mmap", "file"])
            .help("Access account storages using this method"),
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
    ]
    .into_boxed_slice()
}

// For our current version of CLAP, the value passed to Arg::default_value()
// must be a &str. But, we can't convert an integer to a &str at compile time.
// So, declare this constant and enforce equality with the following unit test
// test_max_genesis_archive_unpacked_size_constant
const MAX_GENESIS_ARCHIVE_UNPACKED_SIZE_STR: &str = "10485760";

/// Returns the arguments that configure loading genesis
pub fn load_genesis_arg<'a, 'b>() -> Arg<'a, 'b> {
    Arg::with_name("max_genesis_archive_unpacked_size")
        .long("max-genesis-archive-unpacked-size")
        .value_name("NUMBER")
        .takes_value(true)
        .default_value(MAX_GENESIS_ARCHIVE_UNPACKED_SIZE_STR)
        .help("maximum total uncompressed size of unpacked genesis archive")
}

/// Returns the arguments that configure snapshot loading
pub fn snapshot_args<'a, 'b>() -> Box<[Arg<'a, 'b>]> {
    vec![
        Arg::with_name("no_snapshot")
            .long("no-snapshot")
            .takes_value(false)
            .help("Do not start from a local snapshot if present"),
        Arg::with_name("snapshots")
            .long("snapshots")
            .value_name("DIR")
            .takes_value(true)
            .global(true)
            .help("Use DIR for snapshot location [default: --ledger value]"),
        Arg::with_name("full_snapshot_archive_path")
            .long("full-snapshot-archive-path")
            .value_name("DIR")
            .takes_value(true)
            .global(true)
            .help("Use DIR as full snapshot archives location [default: --snapshots value]"),
        Arg::with_name("incremental_snapshot_archive_path")
            .long("incremental-snapshot-archive-path")
            .value_name("DIR")
            .takes_value(true)
            .global(true)
            .help("Use DIR as incremental snapshot archives location [default: --snapshots value]"),
        Arg::with_name(use_snapshot_archives_at_startup::cli::NAME)
            .long(use_snapshot_archives_at_startup::cli::LONG_ARG)
            .takes_value(true)
            .possible_values(use_snapshot_archives_at_startup::cli::POSSIBLE_VALUES)
            .default_value(use_snapshot_archives_at_startup::cli::default_value_for_ledger_tool())
            .help(use_snapshot_archives_at_startup::cli::HELP)
            .long_help(use_snapshot_archives_at_startup::cli::LONG_HELP),
    ]
    .into_boxed_slice()
}

/// Parse a `ProcessOptions` from subcommand arguments. This function attempts
/// to parse all flags related to `ProcessOptions`; however, subcommands that
/// use this function may not support all flags.
pub fn parse_process_options(ledger_path: &Path, arg_matches: &ArgMatches<'_>) -> ProcessOptions {
    let new_hard_forks = hardforks_of(arg_matches, "hard_forks");
    let accounts_db_config = get_accounts_db_config(ledger_path, arg_matches);
    let log_messages_bytes_limit = value_t!(arg_matches, "log_messages_bytes_limit", usize).ok();
    let runtime_config = RuntimeConfig {
        log_messages_bytes_limit,
        ..RuntimeConfig::default()
    };

    if arg_matches.is_present("skip_poh_verify") {
        eprintln!("--skip-poh-verify is deprecated.  Replace with --skip-verification.");
    }
    let run_verification =
        !(arg_matches.is_present("skip_poh_verify") || arg_matches.is_present("skip_verification"));
    let halt_at_slot = value_t!(arg_matches, "halt_at_slot", Slot).ok();
    let use_snapshot_archives_at_startup = value_t_or_exit!(
        arg_matches,
        use_snapshot_archives_at_startup::cli::NAME,
        UseSnapshotArchivesAtStartup
    );
    let accounts_db_skip_shrink = arg_matches.is_present("accounts_db_skip_shrink");
    let verify_index = arg_matches.is_present("verify_accounts_index");
    let limit_load_slot_count_from_snapshot =
        value_t!(arg_matches, "limit_load_slot_count_from_snapshot", usize).ok();
    let run_final_accounts_hash_calc = arg_matches.is_present("run_final_hash_calc");
    let debug_keys = pubkeys_of(arg_matches, "debug_key")
        .map(|pubkeys| Arc::new(pubkeys.into_iter().collect::<HashSet<_>>()));
    let allow_dead_slots = arg_matches.is_present("allow_dead_slots");
    let abort_on_invalid_block = arg_matches.is_present("abort_on_invalid_block");
    let no_block_cost_limits = arg_matches.is_present("no_block_cost_limits");

    ProcessOptions {
        new_hard_forks,
        runtime_config,
        accounts_db_config,
        accounts_db_skip_shrink,
        verify_index,
        limit_load_slot_count_from_snapshot,
        run_final_accounts_hash_calc,
        debug_keys,
        run_verification,
        allow_dead_slots,
        halt_at_slot,
        use_snapshot_archives_at_startup,
        abort_on_invalid_block,
        no_block_cost_limits,
        ..ProcessOptions::default()
    }
}

// Build an `AccountsDbConfig` from subcommand arguments. All of the arguments
// matched by this functional are either optional or have a default value.
// Thus, a subcommand need not support all of the arguments that are matched
// by this function.
pub fn get_accounts_db_config(
    ledger_path: &Path,
    arg_matches: &ArgMatches<'_>,
) -> AccountsDbConfig {
    let ledger_tool_ledger_path = ledger_path.join(LEDGER_TOOL_DIRECTORY);

    let accounts_index_bins = value_t!(arg_matches, "accounts_index_bins", usize).ok();
    let num_initial_accounts =
        value_t!(arg_matches, "accounts_index_initial_accounts_count", usize).ok();
    let accounts_index_index_limit = if !arg_matches.is_present("enable_accounts_disk_index") {
        IndexLimit::InMemOnly
    } else {
        IndexLimit::Minimal
    };
    let accounts_index_drives = values_t!(arg_matches, "accounts_index_path", String)
        .ok()
        .map(|drives| drives.into_iter().map(PathBuf::from).collect())
        .unwrap_or_else(|| vec![ledger_tool_ledger_path.join("accounts_index")]);
    let accounts_index_config = AccountsIndexConfig {
        bins: accounts_index_bins,
        num_initial_accounts,
        index_limit: accounts_index_index_limit,
        drives: Some(accounts_index_drives),
        ..AccountsIndexConfig::default()
    };

    let storage_access = arg_matches
        .value_of("accounts_db_access_storages_method")
        .map(|method| match method {
            "mmap" => {
                warn!("Using `mmap` for `--accounts-db-access-storages-method` is now deprecated.");
                #[allow(deprecated)]
                StorageAccess::Mmap
            }
            "file" => StorageAccess::File,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts-db-access-storages-method")
            }
        })
        .unwrap_or_default();

    let scan_filter_for_shrinking = arg_matches
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

    AccountsDbConfig {
        index: Some(accounts_index_config),
        base_working_path: Some(ledger_tool_ledger_path),
        ancient_append_vec_offset: value_t!(arg_matches, "accounts_db_ancient_append_vecs", i64)
            .ok(),
        ancient_storage_ideal_size: value_t!(
            arg_matches,
            "accounts_db_ancient_storage_ideal_size",
            u64
        )
        .ok(),
        max_ancient_storages: value_t!(arg_matches, "accounts_db_max_ancient_storages", usize).ok(),
        exhaustively_verify_refcounts: arg_matches.is_present("accounts_db_verify_refcounts"),
        skip_initial_hash_calc: arg_matches.is_present("accounts_db_skip_initial_hash_calculation"),
        storage_access,
        scan_filter_for_shrinking,
        memlock_budget_size: DEFAULT_MEMLOCK_BUDGET_SIZE,
        ..AccountsDbConfig::default()
    }
}

pub(crate) fn parse_encoding_format(matches: &ArgMatches<'_>) -> UiAccountEncoding {
    match matches.value_of("encoding") {
        Some("jsonParsed") => UiAccountEncoding::JsonParsed,
        Some("base64") => UiAccountEncoding::Base64,
        Some("base64+zstd") => UiAccountEncoding::Base64Zstd,
        _ => UiAccountEncoding::Base64,
    }
}

pub(crate) fn parse_account_output_config(matches: &ArgMatches<'_>) -> CliAccountNewConfig {
    let data_encoding = parse_encoding_format(matches);
    let output_account_data = !matches.is_present("no_account_data");
    let data_slice_config = if output_account_data {
        // None yields the entire account in the slice
        None
    } else {
        // usize::MAX is a sentinel that will yield an
        // empty data slice. Because of this, length is
        // ignored so any value will do
        let offset = usize::MAX;
        let length = 0;
        Some(UiDataSliceConfig { offset, length })
    };

    CliAccountNewConfig {
        data_encoding,
        data_slice_config,
        ..CliAccountNewConfig::default()
    }
}

// This function is duplicated in validator/src/main.rs...
pub fn hardforks_of(matches: &ArgMatches<'_>, name: &str) -> Option<Vec<Slot>> {
    if matches.is_present(name) {
        Some(values_t_or_exit!(matches, name, Slot))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_genesis_utils::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE};

    #[test]
    fn test_max_genesis_archive_unpacked_size_constant() {
        assert_eq!(
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE_STR
                .parse::<u64>()
                .unwrap()
        );
    }
}
