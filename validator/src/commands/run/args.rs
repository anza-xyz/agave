use {
    crate::cli::{hash_validator, port_range_validator, port_validator, DefaultArgs},
    clap::{App, Arg},
    solana_clap_utils::{
        hidden_unless_forced,
        input_validators::{
            is_keypair_or_ask_keyword, is_parsable, is_pow2, is_pubkey, is_pubkey_or_keypair,
            is_slot, is_within_range, validate_maximum_full_snapshot_archives_to_retain,
            validate_maximum_incremental_snapshot_archives_to_retain,
        },
        keypair::SKIP_SEED_PHRASE_VALIDATION_ARG,
    },
    solana_core::{
        banking_trace::DirByteLimit,
        validator::{BlockProductionMethod, BlockVerificationMethod, TransactionStructure},
    },
    solana_ledger::use_snapshot_archives_at_startup,
    solana_runtime::snapshot_utils::{SnapshotVersion, SUPPORTED_ARCHIVE_COMPRESSION},
    solana_send_transaction_service::send_transaction_service::{
        MAX_BATCH_SEND_RATE_MS, MAX_TRANSACTION_BATCH_SIZE,
    },
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    std::str::FromStr,
};

const EXCLUDE_KEY: &str = "account-index-exclude-key";
const INCLUDE_KEY: &str = "account-index-include-key";

pub fn add_args<'a>(app: App<'a, 'a>, default_args: &'a DefaultArgs) -> App<'a, 'a> {
    app
    .arg(
        Arg::with_name(SKIP_SEED_PHRASE_VALIDATION_ARG.name)
            .long(SKIP_SEED_PHRASE_VALIDATION_ARG.long)
            .help(SKIP_SEED_PHRASE_VALIDATION_ARG.help),
    )
    .arg(
        Arg::with_name("identity")
            .short("i")
            .long("identity")
            .value_name("KEYPAIR")
            .takes_value(true)
            .validator(is_keypair_or_ask_keyword)
            .help("Validator identity keypair"),
    )
    .arg(
        Arg::with_name("authorized_voter_keypairs")
            .long("authorized-voter")
            .value_name("KEYPAIR")
            .takes_value(true)
            .validator(is_keypair_or_ask_keyword)
            .requires("vote_account")
            .multiple(true)
            .help(
                "Include an additional authorized voter keypair. May be specified multiple \
                 times. [default: the --identity keypair]",
            ),
    )
    .arg(
        Arg::with_name("vote_account")
            .long("vote-account")
            .value_name("ADDRESS")
            .takes_value(true)
            .validator(is_pubkey_or_keypair)
            .requires("identity")
            .help(
                "Validator vote account public key. If unspecified, voting will be disabled. \
                 The authorized voter for the account must either be the --identity keypair \
                 or set by the --authorized-voter argument",
            ),
    )
    .arg(
        Arg::with_name("init_complete_file")
            .long("init-complete-file")
            .value_name("FILE")
            .takes_value(true)
            .help(
                "Create this file if it doesn't already exist once validator initialization \
                 is complete",
            ),
    )
    .arg(
        Arg::with_name("ledger_path")
            .short("l")
            .long("ledger")
            .value_name("DIR")
            .takes_value(true)
            .required(true)
            .default_value(&default_args.ledger_path)
            .help("Use DIR as ledger location"),
    )
    .arg(
        Arg::with_name("entrypoint")
            .short("n")
            .long("entrypoint")
            .value_name("HOST:PORT")
            .takes_value(true)
            .multiple(true)
            .validator(solana_net_utils::is_host_port)
            .help("Rendezvous with the cluster at this gossip entrypoint"),
    )
    .arg(
        Arg::with_name("no_snapshot_fetch")
            .long("no-snapshot-fetch")
            .takes_value(false)
            .help(
                "Do not attempt to fetch a snapshot from the cluster, start from a local \
                 snapshot if present",
            ),
    )
    .arg(
        Arg::with_name("no_genesis_fetch")
            .long("no-genesis-fetch")
            .takes_value(false)
            .help("Do not fetch genesis from the cluster"),
    )
    .arg(
        Arg::with_name("no_voting")
            .long("no-voting")
            .takes_value(false)
            .help("Launch validator without voting"),
    )
    .arg(
        Arg::with_name("check_vote_account")
            .long("check-vote-account")
            .takes_value(true)
            .value_name("RPC_URL")
            .requires("entrypoint")
            .conflicts_with_all(&["no_check_vote_account", "no_voting"])
            .help(
                "Sanity check vote account state at startup. The JSON RPC endpoint at RPC_URL \
                 must expose `--full-rpc-api`",
            ),
    )
    .arg(
        Arg::with_name("restricted_repair_only_mode")
            .long("restricted-repair-only-mode")
            .takes_value(false)
            .help(
                "Do not publish the Gossip, TPU, TVU or Repair Service ports. Doing so causes \
                 the node to operate in a limited capacity that reduces its exposure to the \
                 rest of the cluster. The --no-voting flag is implicit when this flag is \
                 enabled",
            ),
    )
    .arg(
        Arg::with_name("dev_halt_at_slot")
            .long("dev-halt-at-slot")
            .value_name("SLOT")
            .validator(is_slot)
            .takes_value(true)
            .help("Halt the validator when it reaches the given slot"),
    )
    .arg(
        Arg::with_name("rpc_port")
            .long("rpc-port")
            .value_name("PORT")
            .takes_value(true)
            .validator(port_validator)
            .help("Enable JSON RPC on this port, and the next port for the RPC websocket"),
    )
    .arg(
        Arg::with_name("full_rpc_api")
            .long("full-rpc-api")
            .conflicts_with("minimal_rpc_api")
            .takes_value(false)
            .help("Expose RPC methods for querying chain state and transaction history"),
    )
    .arg(
        Arg::with_name("private_rpc")
            .long("private-rpc")
            .takes_value(false)
            .help("Do not publish the RPC port for use by others"),
    )
    .arg(
        Arg::with_name("no_port_check")
            .long("no-port-check")
            .takes_value(false)
            .hidden(hidden_unless_forced())
            .help("Do not perform TCP/UDP reachable port checks at start-up"),
    )
    .arg(
        Arg::with_name("enable_rpc_transaction_history")
            .long("enable-rpc-transaction-history")
            .takes_value(false)
            .help(
                "Enable historical transaction info over JSON RPC, including the \
                 'getConfirmedBlock' API. This will cause an increase in disk usage and IOPS",
            ),
    )
    .arg(
        Arg::with_name("enable_rpc_bigtable_ledger_storage")
            .long("enable-rpc-bigtable-ledger-storage")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help(
                "Fetch historical transaction info from a BigTable instance as a fallback to \
                 local ledger data",
            ),
    )
    .arg(
        Arg::with_name("enable_bigtable_ledger_upload")
            .long("enable-bigtable-ledger-upload")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help("Upload new confirmed blocks into a BigTable instance"),
    )
    .arg(
        Arg::with_name("enable_extended_tx_metadata_storage")
            .long("enable-extended-tx-metadata-storage")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help(
                "Include CPI inner instructions, logs, and return data in the historical \
                 transaction info stored",
            ),
    )
    .arg(
        Arg::with_name("rpc_max_multiple_accounts")
            .long("rpc-max-multiple-accounts")
            .value_name("MAX ACCOUNTS")
            .takes_value(true)
            .default_value(&default_args.rpc_max_multiple_accounts)
            .help(
                "Override the default maximum accounts accepted by the getMultipleAccounts \
                 JSON RPC method",
            ),
    )
    .arg(
        Arg::with_name("health_check_slot_distance")
            .long("health-check-slot-distance")
            .value_name("SLOT_DISTANCE")
            .takes_value(true)
            .default_value(&default_args.health_check_slot_distance)
            .help(
                "Report this validator as healthy if its latest replayed optimistically \
                 confirmed slot is within the specified number of slots from the cluster's \
                 latest optimistically confirmed slot",
            ),
    )
    .arg(
        Arg::with_name("skip_preflight_health_check")
            .long("skip-preflight-health-check")
            .takes_value(false)
            .help(
                "Skip health check when running a preflight check",
            ),
    )
    .arg(
        Arg::with_name("rpc_faucet_addr")
            .long("rpc-faucet-address")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help("Enable the JSON RPC 'requestAirdrop' API with this faucet address."),
    )
    .arg(
        Arg::with_name("account_paths")
            .long("accounts")
            .value_name("PATHS")
            .takes_value(true)
            .multiple(true)
            .help(
                "Comma separated persistent accounts location. \
                May be specified multiple times. \
                [default: <LEDGER>/accounts]",
            ),
    )
    .arg(
        Arg::with_name("account_shrink_path")
            .long("account-shrink-path")
            .value_name("PATH")
            .takes_value(true)
            .multiple(true)
            .help("Path to accounts shrink path which can hold a compacted account set."),
    )
    .arg(
        Arg::with_name("accounts_hash_cache_path")
            .long("accounts-hash-cache-path")
            .value_name("PATH")
            .takes_value(true)
            .help(
                "Use PATH as accounts hash cache location \
                 [default: <LEDGER>/accounts_hash_cache]",
            ),
    )
    .arg(
        Arg::with_name("snapshots")
            .long("snapshots")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as the base location for snapshots.")
            .long_help(
                "Use DIR as the base location for snapshots. \
                 Snapshot archives will use DIR unless --full-snapshot-archive-path or \
                 --incremental-snapshot-archive-path is specified. \
                 Additionally, a subdirectory named \"snapshots\" will be created in DIR. \
                 This subdirectory holds internal files/data that are used when generating \
                 snapshot archives. \
                 [default: --ledger value]",
             ),
    )
    .arg(
        Arg::with_name(use_snapshot_archives_at_startup::cli::NAME)
            .long(use_snapshot_archives_at_startup::cli::LONG_ARG)
            .takes_value(true)
            .possible_values(use_snapshot_archives_at_startup::cli::POSSIBLE_VALUES)
            .default_value(use_snapshot_archives_at_startup::cli::default_value())
            .help(use_snapshot_archives_at_startup::cli::HELP)
            .long_help(use_snapshot_archives_at_startup::cli::LONG_HELP),
    )
    .arg(
        Arg::with_name("full_snapshot_archive_path")
            .long("full-snapshot-archive-path")
            .value_name("DIR")
            .takes_value(true)
            .help(
                "Use DIR as full snapshot archives location \
                 [default: --snapshots value]",
             ),
    )
    .arg(
        Arg::with_name("incremental_snapshot_archive_path")
            .long("incremental-snapshot-archive-path")
            .conflicts_with("no-incremental-snapshots")
            .value_name("DIR")
            .takes_value(true)
            .help(
                "Use DIR as incremental snapshot archives location \
                 [default: --snapshots value]",
            ),
    )
    .arg(
        Arg::with_name("tower")
            .long("tower")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as file tower storage location [default: --ledger value]"),
    )
    .arg(
        Arg::with_name("gossip_port")
            .long("gossip-port")
            .value_name("PORT")
            .takes_value(true)
            .help("Gossip port number for the validator"),
    )
    .arg(
        Arg::with_name("gossip_host")
            .long("gossip-host")
            .value_name("HOST")
            .takes_value(true)
            .validator(solana_net_utils::is_host)
            .help(
                "Gossip DNS name or IP address for the validator to advertise in gossip \
                 [default: ask --entrypoint, or 127.0.0.1 when --entrypoint is not provided]",
            ),
    )
    .arg(
        Arg::with_name("public_tpu_addr")
            .long("public-tpu-address")
            .alias("tpu-host-addr")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help(
                "Specify TPU address to advertise in gossip \
                 [default: ask --entrypoint or localhost when --entrypoint is not provided]",
            ),
    )
    .arg(
        Arg::with_name("public_tpu_forwards_addr")
            .long("public-tpu-forwards-address")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help(
                "Specify TPU Forwards address to advertise in gossip [default: ask \
                 --entrypoint or localhostwhen --entrypoint is not provided]",
            ),
    )
    .arg(
        Arg::with_name("public_rpc_addr")
            .long("public-rpc-address")
            .value_name("HOST:PORT")
            .takes_value(true)
            .conflicts_with("private_rpc")
            .validator(solana_net_utils::is_host_port)
            .help(
                "RPC address for the validator to advertise publicly in gossip. Useful for \
                 validators running behind a load balancer or proxy [default: use \
                 --rpc-bind-address / --rpc-port]",
            ),
    )
    .arg(
        Arg::with_name("dynamic_port_range")
            .long("dynamic-port-range")
            .value_name("MIN_PORT-MAX_PORT")
            .takes_value(true)
            .default_value(&default_args.dynamic_port_range)
            .validator(port_range_validator)
            .help("Range to use for dynamically assigned ports"),
    )
    .arg(
        Arg::with_name("maximum_local_snapshot_age")
            .long("maximum-local-snapshot-age")
            .value_name("NUMBER_OF_SLOTS")
            .takes_value(true)
            .default_value(&default_args.maximum_local_snapshot_age)
            .help(
                "Reuse a local snapshot if it's less than this many slots behind the highest \
                 snapshot available for download from other validators",
            ),
    )
    .arg(
        Arg::with_name("no_snapshots")
            .long("no-snapshots")
            .takes_value(false)
            .conflicts_with_all(&["no_incremental_snapshots", "snapshot_interval_slots", "full_snapshot_interval_slots"])
            .help("Disable all snapshot generation")
    )
    .arg(
        Arg::with_name("no_incremental_snapshots")
            .long("no-incremental-snapshots")
            .takes_value(false)
            .help("Disable incremental snapshots")
    )
    .arg(
        Arg::with_name("snapshot_interval_slots")
            .long("snapshot-interval-slots")
            .alias("incremental-snapshot-interval-slots")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.incremental_snapshot_archive_interval_slots)
            .help("Number of slots between generating snapshots")
            .long_help(
                "Number of slots between generating snapshots. \
                 If incremental snapshots are enabled, this sets the incremental snapshot interval. \
                 If incremental snapshots are disabled, this sets the full snapshot interval. \
                 To disable all snapshot generation, see --no-snapshots.",
            ),
    )
    .arg(
        Arg::with_name("full_snapshot_interval_slots")
            .long("full-snapshot-interval-slots")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.full_snapshot_archive_interval_slots)
            .help("Number of slots between generating full snapshots")
            .long_help(
                "Number of slots between generating full snapshots. Must be a multiple of the \
                 incremental snapshot interval. Only used when incremental snapshots are enabled.",
            ),
    )
    .arg(
        Arg::with_name("maximum_full_snapshots_to_retain")
            .long("maximum-full-snapshots-to-retain")
            .alias("maximum-snapshots-to-retain")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.maximum_full_snapshot_archives_to_retain)
            .validator(validate_maximum_full_snapshot_archives_to_retain)
            .help(
                "The maximum number of full snapshot archives to hold on to when purging \
                 older snapshots.",
            ),
    )
    .arg(
        Arg::with_name("maximum_incremental_snapshots_to_retain")
            .long("maximum-incremental-snapshots-to-retain")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.maximum_incremental_snapshot_archives_to_retain)
            .validator(validate_maximum_incremental_snapshot_archives_to_retain)
            .help(
                "The maximum number of incremental snapshot archives to hold on to when \
                 purging older snapshots.",
            ),
    )
    .arg(
        Arg::with_name("snapshot_packager_niceness_adj")
            .long("snapshot-packager-niceness-adjustment")
            .value_name("ADJUSTMENT")
            .takes_value(true)
            .validator(solana_perf::thread::is_niceness_adjustment_valid)
            .default_value(&default_args.snapshot_packager_niceness_adjustment)
            .help(
                "Add this value to niceness of snapshot packager thread. Negative value \
                 increases priority, positive value decreases priority.",
            ),
    )
    .arg(
        Arg::with_name("minimal_snapshot_download_speed")
            .long("minimal-snapshot-download-speed")
            .value_name("MINIMAL_SNAPSHOT_DOWNLOAD_SPEED")
            .takes_value(true)
            .default_value(&default_args.min_snapshot_download_speed)
            .help(
                "The minimal speed of snapshot downloads measured in bytes/second. If the \
                 initial download speed falls below this threshold, the system will retry the \
                 download against a different rpc node.",
            ),
    )
    .arg(
        Arg::with_name("maximum_snapshot_download_abort")
            .long("maximum-snapshot-download-abort")
            .value_name("MAXIMUM_SNAPSHOT_DOWNLOAD_ABORT")
            .takes_value(true)
            .default_value(&default_args.max_snapshot_download_abort)
            .help(
                "The maximum number of times to abort and retry when encountering a slow \
                 snapshot download.",
            ),
    )
    .arg(
        Arg::with_name("contact_debug_interval")
            .long("contact-debug-interval")
            .value_name("CONTACT_DEBUG_INTERVAL")
            .takes_value(true)
            .default_value(&default_args.contact_debug_interval)
            .help("Milliseconds between printing contact debug from gossip."),
    )
    .arg(
        Arg::with_name("no_poh_speed_test")
            .long("no-poh-speed-test")
            .hidden(hidden_unless_forced())
            .help("Skip the check for PoH speed."),
    )
    .arg(
        Arg::with_name("no_os_network_limits_test")
            .hidden(hidden_unless_forced())
            .long("no-os-network-limits-test")
            .help("Skip checks for OS network limits."),
    )
    .arg(
        Arg::with_name("no_os_memory_stats_reporting")
            .long("no-os-memory-stats-reporting")
            .hidden(hidden_unless_forced())
            .help("Disable reporting of OS memory statistics."),
    )
    .arg(
        Arg::with_name("no_os_network_stats_reporting")
            .long("no-os-network-stats-reporting")
            .hidden(hidden_unless_forced())
            .help("Disable reporting of OS network statistics."),
    )
    .arg(
        Arg::with_name("no_os_cpu_stats_reporting")
            .long("no-os-cpu-stats-reporting")
            .hidden(hidden_unless_forced())
            .help("Disable reporting of OS CPU statistics."),
    )
    .arg(
        Arg::with_name("no_os_disk_stats_reporting")
            .long("no-os-disk-stats-reporting")
            .hidden(hidden_unless_forced())
            .help("Disable reporting of OS disk statistics."),
    )
    .arg(
        Arg::with_name("snapshot_version")
            .long("snapshot-version")
            .value_name("SNAPSHOT_VERSION")
            .validator(is_parsable::<SnapshotVersion>)
            .takes_value(true)
            .default_value(default_args.snapshot_version.into())
            .help("Output snapshot version"),
    )
    .arg(
        Arg::with_name("limit_ledger_size")
            .long("limit-ledger-size")
            .value_name("SHRED_COUNT")
            .takes_value(true)
            .min_values(0)
            .max_values(1)
            /* .default_value() intentionally not used here! */
            .help("Keep this amount of shreds in root slots."),
    )
    .arg(
        Arg::with_name("rocksdb_shred_compaction")
            .long("rocksdb-shred-compaction")
            .value_name("ROCKSDB_COMPACTION_STYLE")
            .takes_value(true)
            .possible_values(&["level"])
            .default_value(&default_args.rocksdb_shred_compaction)
            .help(
                "Controls how RocksDB compacts shreds. *WARNING*: You will lose your \
                 Blockstore data when you switch between options. Possible values are: \
                 'level': stores shreds using RocksDB's default (level) compaction.",
            ),
    )
    .arg(
        Arg::with_name("rocksdb_ledger_compression")
            .hidden(hidden_unless_forced())
            .long("rocksdb-ledger-compression")
            .value_name("COMPRESSION_TYPE")
            .takes_value(true)
            .possible_values(&["none", "lz4", "snappy", "zlib"])
            .default_value(&default_args.rocksdb_ledger_compression)
            .help(
                "The compression algorithm that is used to compress transaction status data. \
                 Turning on compression can save ~10% of the ledger size.",
            ),
    )
    .arg(
        Arg::with_name("rocksdb_perf_sample_interval")
            .hidden(hidden_unless_forced())
            .long("rocksdb-perf-sample-interval")
            .value_name("ROCKS_PERF_SAMPLE_INTERVAL")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rocksdb_perf_sample_interval)
            .help(
                "Controls how often RocksDB read/write performance samples are collected. \
                 Perf samples are collected in 1 / ROCKS_PERF_SAMPLE_INTERVAL sampling rate.",
            ),
    )
    .arg(
        Arg::with_name("skip_startup_ledger_verification")
            .long("skip-startup-ledger-verification")
            .takes_value(false)
            .help("Skip ledger verification at validator bootup."),
    )
    .arg(
        Arg::with_name("cuda")
            .long("cuda")
            .takes_value(false)
            .help("Use CUDA"),
    )
    .arg(
        clap::Arg::with_name("require_tower")
            .long("require-tower")
            .takes_value(false)
            .help("Refuse to start if saved tower state is not found"),
    )
    .arg(
        Arg::with_name("expected_genesis_hash")
            .long("expected-genesis-hash")
            .value_name("HASH")
            .takes_value(true)
            .validator(hash_validator)
            .help("Require the genesis have this hash"),
    )
    .arg(
        Arg::with_name("expected_bank_hash")
            .long("expected-bank-hash")
            .value_name("HASH")
            .takes_value(true)
            .validator(hash_validator)
            .help("When wait-for-supermajority <x>, require the bank at <x> to have this hash"),
    )
    .arg(
        Arg::with_name("expected_shred_version")
            .long("expected-shred-version")
            .value_name("VERSION")
            .takes_value(true)
            .validator(is_parsable::<u16>)
            .help("Require the shred version be this value"),
    )
    .arg(
        Arg::with_name("logfile")
            .short("o")
            .long("log")
            .value_name("FILE")
            .takes_value(true)
            .help(
                "Redirect logging to the specified file, '-' for standard error. Sending the \
                 SIGUSR1 signal to the validator process will cause it to re-open the log file",
            ),
    )
    .arg(
        Arg::with_name("wait_for_supermajority")
            .long("wait-for-supermajority")
            .requires("expected_bank_hash")
            .requires("expected_shred_version")
            .value_name("SLOT")
            .validator(is_slot)
            .help(
                "After processing the ledger and the next slot is SLOT, wait until a \
                 supermajority of stake is visible on gossip before starting PoH",
            ),
    )
    .arg(
        Arg::with_name("no_wait_for_vote_to_start_leader")
            .hidden(hidden_unless_forced())
            .long("no-wait-for-vote-to-start-leader")
            .help(
                "If the validator starts up with no ledger, it will wait to start block \
                 production until it sees a vote land in a rooted slot. This prevents \
                 double signing. Turn off to risk double signing a block.",
            ),
    )
    .arg(
        Arg::with_name("hard_forks")
            .long("hard-fork")
            .value_name("SLOT")
            .validator(is_slot)
            .multiple(true)
            .takes_value(true)
            .help("Add a hard fork at this slot"),
    )
    .arg(
        Arg::with_name("known_validators")
            .alias("trusted-validator")
            .long("known-validator")
            .validator(is_pubkey)
            .value_name("VALIDATOR IDENTITY")
            .multiple(true)
            .takes_value(true)
            .help(
                "A snapshot hash must be published in gossip by this validator to be \
                 accepted. May be specified multiple times. If unspecified any snapshot hash \
                 will be accepted",
            ),
    )
    .arg(
        Arg::with_name("debug_key")
            .long("debug-key")
            .validator(is_pubkey)
            .value_name("ADDRESS")
            .multiple(true)
            .takes_value(true)
            .help("Log when transactions are processed which reference a given key."),
    )
    .arg(
        Arg::with_name("only_known_rpc")
            .alias("no-untrusted-rpc")
            .long("only-known-rpc")
            .takes_value(false)
            .requires("known_validators")
            .help("Use the RPC service of known validators only"),
    )
    .arg(
        Arg::with_name("repair_validators")
            .long("repair-validator")
            .validator(is_pubkey)
            .value_name("VALIDATOR IDENTITY")
            .multiple(true)
            .takes_value(true)
            .help(
                "A list of validators to request repairs from. If specified, repair will not \
                 request from validators outside this set [default: all validators]",
            ),
    )
    .arg(
        Arg::with_name("repair_whitelist")
            .hidden(hidden_unless_forced())
            .long("repair-whitelist")
            .validator(is_pubkey)
            .value_name("VALIDATOR IDENTITY")
            .multiple(true)
            .takes_value(true)
            .help(
                "A list of validators to prioritize repairs from. If specified, repair \
                 requests from validators in the list will be prioritized over requests from \
                 other validators. [default: all validators]",
            ),
    )
    .arg(
        Arg::with_name("gossip_validators")
            .long("gossip-validator")
            .validator(is_pubkey)
            .value_name("VALIDATOR IDENTITY")
            .multiple(true)
            .takes_value(true)
            .help(
                "A list of validators to gossip with. If specified, gossip will not \
                 push/pull from from validators outside this set. [default: all validators]",
            ),
    )
    .arg(
        Arg::with_name("tpu_coalesce_ms")
            .long("tpu-coalesce-ms")
            .value_name("MILLISECS")
            .takes_value(true)
            .validator(is_parsable::<u64>)
            .help("Milliseconds to wait in the TPU receiver for packet coalescing."),
    )
    .arg(
        Arg::with_name("tpu_use_quic")
            .long("tpu-use-quic")
            .takes_value(false)
            .hidden(hidden_unless_forced())
            .conflicts_with("tpu_disable_quic")
            .help("Use QUIC to send transactions."),
    )
    .arg(
        Arg::with_name("tpu_disable_quic")
            .long("tpu-disable-quic")
            .takes_value(false)
            .help("Do not use QUIC to send transactions."),
    )
    .arg(
        Arg::with_name("tpu_enable_udp")
            .long("tpu-enable-udp")
            .takes_value(false)
            .help("Enable UDP for receiving/sending transactions."),
    )
    .arg(
        Arg::with_name("tpu_connection_pool_size")
            .long("tpu-connection-pool-size")
            .takes_value(true)
            .default_value(&default_args.tpu_connection_pool_size)
            .validator(is_parsable::<usize>)
            .help("Controls the TPU connection pool size per remote address"),
    )
    .arg(
        Arg::with_name("tpu_max_connections_per_ipaddr_per_minute")
            .long("tpu-max-connections-per-ipaddr-per-minute")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_ipaddr_per_minute)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the rate of the clients connections per IpAddr per minute."),
    )
    .arg(
        Arg::with_name("vote_use_quic")
            .long("vote-use-quic")
            .takes_value(true)
            .default_value(&default_args.vote_use_quic)
            .hidden(hidden_unless_forced())
            .help("Controls if to use QUIC to send votes."),
    )
    .arg(
        Arg::with_name("tpu_max_connections_per_peer")
            .long("tpu-max-connections-per-peer")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_peer)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections per IpAddr."),
    )
    .arg(
        Arg::with_name("tpu_max_staked_connections")
            .long("tpu-max-staked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_staked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections for TPU from staked nodes."),
    )
    .arg(
        Arg::with_name("tpu_max_unstaked_connections")
            .long("tpu-max-unstaked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_unstaked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections fort TPU from unstaked nodes."),
    )
    .arg(
        Arg::with_name("tpu_max_fwd_staked_connections")
            .long("tpu-max-fwd-staked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_fwd_staked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections for TPU-forward from staked nodes."),
    )
    .arg(
        Arg::with_name("tpu_max_fwd_unstaked_connections")
            .long("tpu-max-fwd-unstaked-connections")
            .takes_value(true)
            .default_value(&default_args.tpu_max_fwd_unstaked_connections)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections for TPU-forward from unstaked nodes."),
    )
    .arg(
        Arg::with_name("tpu_max_streams_per_ms")
            .long("tpu-max-streams-per-ms")
            .takes_value(true)
            .default_value(&default_args.tpu_max_streams_per_ms)
            .validator(is_parsable::<usize>)
            .hidden(hidden_unless_forced())
            .help("Controls the max number of streams for a TPU service."),
    )
    .arg(
        Arg::with_name("num_quic_endpoints")
            .long("num-quic-endpoints")
            .takes_value(true)
            .default_value(&default_args.num_quic_endpoints)
            .validator(is_parsable::<usize>)
            .hidden(hidden_unless_forced())
            .help("The number of QUIC endpoints used for TPU and TPU-Forward. It can be increased to \
                   increase network ingest throughput, at the expense of higher CPU and general \
                   validator load."),
    )
    .arg(
        Arg::with_name("staked_nodes_overrides")
            .long("staked-nodes-overrides")
            .value_name("PATH")
            .takes_value(true)
            .help(
                "Provide path to a yaml file with custom overrides for stakes of specific \
                 identities. Overriding the amount of stake this validator considers as valid \
                 for other peers in network. The stake amount is used for calculating the \
                 number of QUIC streams permitted from the peer and vote packet sender stage. \
                 Format of the file: `staked_map_id: {<pubkey>: <SOL stake amount>}",
            ),
    )
    .arg(
        Arg::with_name("bind_address")
            .long("bind-address")
            .value_name("HOST")
            .takes_value(true)
            .validator(solana_net_utils::is_host)
            .default_value(&default_args.bind_address)
            .help("IP address to bind the validator ports"),
    )
    .arg(
        Arg::with_name("rpc_bind_address")
            .long("rpc-bind-address")
            .value_name("HOST")
            .takes_value(true)
            .validator(solana_net_utils::is_host)
            .help(
                "IP address to bind the RPC port [default: 127.0.0.1 if --private-rpc is \
                 present, otherwise use --bind-address]",
            ),
    )
    .arg(
        Arg::with_name("rpc_threads")
            .long("rpc-threads")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .default_value(&default_args.rpc_threads)
            .help("Number of threads to use for servicing RPC requests"),
    )
    .arg(
        Arg::with_name("rpc_blocking_threads")
            .long("rpc-blocking-threads")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .validator(|value| {
                value
                    .parse::<u64>()
                    .map_err(|err| format!("error parsing '{value}': {err}"))
                    .and_then(|threads| {
                        if threads > 0 {
                            Ok(())
                        } else {
                            Err("value must be >= 1".to_string())
                        }
                    })
            })
            .takes_value(true)
            .default_value(&default_args.rpc_blocking_threads)
            .help("Number of blocking threads to use for servicing CPU bound RPC requests (eg getMultipleAccounts)"),
    )
    .arg(
        Arg::with_name("rpc_niceness_adj")
            .long("rpc-niceness-adjustment")
            .value_name("ADJUSTMENT")
            .takes_value(true)
            .validator(solana_perf::thread::is_niceness_adjustment_valid)
            .default_value(&default_args.rpc_niceness_adjustment)
            .help(
                "Add this value to niceness of RPC threads. Negative value increases \
                 priority, positive value decreases priority.",
            ),
    )
    .arg(
        Arg::with_name("rpc_bigtable_timeout")
            .long("rpc-bigtable-timeout")
            .value_name("SECONDS")
            .validator(is_parsable::<u64>)
            .takes_value(true)
            .default_value(&default_args.rpc_bigtable_timeout)
            .help("Number of seconds before timing out RPC requests backed by BigTable"),
    )
    .arg(
        Arg::with_name("rpc_bigtable_instance_name")
            .long("rpc-bigtable-instance-name")
            .takes_value(true)
            .value_name("INSTANCE_NAME")
            .default_value(&default_args.rpc_bigtable_instance_name)
            .help("Name of the Bigtable instance to upload to"),
    )
    .arg(
        Arg::with_name("rpc_bigtable_app_profile_id")
            .long("rpc-bigtable-app-profile-id")
            .takes_value(true)
            .value_name("APP_PROFILE_ID")
            .default_value(&default_args.rpc_bigtable_app_profile_id)
            .help("Bigtable application profile id to use in requests"),
    )
    .arg(
        Arg::with_name("rpc_bigtable_max_message_size")
            .long("rpc-bigtable-max-message-size")
            .value_name("BYTES")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .default_value(&default_args.rpc_bigtable_max_message_size)
            .help("Max encoding and decoding message size used in Bigtable Grpc client"),
    )
    .arg(
        Arg::with_name("rpc_pubsub_worker_threads")
            .long("rpc-pubsub-worker-threads")
            .takes_value(true)
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_pubsub_worker_threads)
            .help("PubSub worker threads"),
    )
    .arg(
        Arg::with_name("rpc_pubsub_enable_block_subscription")
            .long("rpc-pubsub-enable-block-subscription")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help("Enable the unstable RPC PubSub `blockSubscribe` subscription"),
    )
    .arg(
        Arg::with_name("rpc_pubsub_enable_vote_subscription")
            .long("rpc-pubsub-enable-vote-subscription")
            .takes_value(false)
            .help("Enable the unstable RPC PubSub `voteSubscribe` subscription"),
    )
    .arg(
        Arg::with_name("rpc_pubsub_max_active_subscriptions")
            .long("rpc-pubsub-max-active-subscriptions")
            .takes_value(true)
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_pubsub_max_active_subscriptions)
            .help(
                "The maximum number of active subscriptions that RPC PubSub will accept \
                 across all connections.",
            ),
    )
    .arg(
        Arg::with_name("rpc_pubsub_queue_capacity_items")
            .long("rpc-pubsub-queue-capacity-items")
            .takes_value(true)
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_pubsub_queue_capacity_items)
            .help(
                "The maximum number of notifications that RPC PubSub will store across all \
                 connections.",
            ),
    )
    .arg(
        Arg::with_name("rpc_pubsub_queue_capacity_bytes")
            .long("rpc-pubsub-queue-capacity-bytes")
            .takes_value(true)
            .value_name("BYTES")
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_pubsub_queue_capacity_bytes)
            .help(
                "The maximum total size of notifications that RPC PubSub will store across \
                 all connections.",
            ),
    )
    .arg(
        Arg::with_name("rpc_pubsub_notification_threads")
            .long("rpc-pubsub-notification-threads")
            .requires("full_rpc_api")
            .takes_value(true)
            .value_name("NUM_THREADS")
            .validator(is_parsable::<usize>)
            .default_value_if(
                "full_rpc_api",
                None,
                &default_args.rpc_pubsub_notification_threads,
            )
            .help(
                "The maximum number of threads that RPC PubSub will use for generating \
                 notifications. 0 will disable RPC PubSub notifications",
            ),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_retry_ms")
            .long("rpc-send-retry-ms")
            .value_name("MILLISECS")
            .takes_value(true)
            .validator(is_parsable::<u64>)
            .default_value(&default_args.rpc_send_transaction_retry_ms)
            .help("The rate at which transactions sent via rpc service are retried."),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_batch_ms")
            .long("rpc-send-batch-ms")
            .value_name("MILLISECS")
            .hidden(hidden_unless_forced())
            .takes_value(true)
            .validator(|s| is_within_range(s, 1..=MAX_BATCH_SEND_RATE_MS))
            .default_value(&default_args.rpc_send_transaction_batch_ms)
            .help("The rate at which transactions sent via rpc service are sent in batch."),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_leader_forward_count")
            .long("rpc-send-leader-count")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<u64>)
            .default_value(&default_args.rpc_send_transaction_leader_forward_count)
            .help(
                "The number of upcoming leaders to which to forward transactions sent via rpc \
                 service.",
            ),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_default_max_retries")
            .long("rpc-send-default-max-retries")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .help(
                "The maximum number of transaction broadcast retries when unspecified by the \
                 request, otherwise retried until expiration.",
            ),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_service_max_retries")
            .long("rpc-send-service-max-retries")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_send_transaction_service_max_retries)
            .help(
                "The maximum number of transaction broadcast retries, regardless of requested \
                 value.",
            ),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_batch_size")
            .long("rpc-send-batch-size")
            .value_name("NUMBER")
            .hidden(hidden_unless_forced())
            .takes_value(true)
            .validator(|s| is_within_range(s, 1..=MAX_TRANSACTION_BATCH_SIZE))
            .default_value(&default_args.rpc_send_transaction_batch_size)
            .help("The size of transactions to be sent in batch."),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_retry_pool_max_size")
            .long("rpc-send-transaction-retry-pool-max-size")
            .value_name("NUMBER")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_send_transaction_retry_pool_max_size)
            .help("The maximum size of transactions retry pool."),
    )
    .arg(
        Arg::with_name("rpc_send_transaction_tpu_peer")
            .long("rpc-send-transaction-tpu-peer")
            .takes_value(true)
            .number_of_values(1)
            .multiple(true)
            .value_name("HOST:PORT")
            .validator(solana_net_utils::is_host_port)
            .help("Peer(s) to broadcast transactions to instead of the current leader")
    )
    .arg(
        Arg::with_name("rpc_send_transaction_also_leader")
            .long("rpc-send-transaction-also-leader")
            .requires("rpc_send_transaction_tpu_peer")
            .help("With `--rpc-send-transaction-tpu-peer HOST:PORT`, also send to the current leader")
    )
    .arg(
        Arg::with_name("rpc_scan_and_fix_roots")
            .long("rpc-scan-and-fix-roots")
            .takes_value(false)
            .requires("enable_rpc_transaction_history")
            .help("Verifies blockstore roots on boot and fixes any gaps"),
    )
    .arg(
        Arg::with_name("rpc_max_request_body_size")
            .long("rpc-max-request-body-size")
            .value_name("BYTES")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .default_value(&default_args.rpc_max_request_body_size)
            .help("The maximum request body size accepted by rpc service"),
    )
    .arg(
        Arg::with_name("geyser_plugin_config")
            .long("geyser-plugin-config")
            .alias("accountsdb-plugin-config")
            .value_name("FILE")
            .takes_value(true)
            .multiple(true)
            .help("Specify the configuration file for the Geyser plugin."),
    )
    .arg(
        Arg::with_name("geyser_plugin_always_enabled")
            .long("geyser-plugin-always-enabled")
            .value_name("BOOLEAN")
            .takes_value(false)
            .help("Еnable Geyser interface even if no Geyser configs are specified."),
    )
    .arg(
        Arg::with_name("snapshot_archive_format")
            .long("snapshot-archive-format")
            .alias("snapshot-compression") // Legacy name used by Solana v1.5.x and older
            .possible_values(SUPPORTED_ARCHIVE_COMPRESSION)
            .default_value(&default_args.snapshot_archive_format)
            .value_name("ARCHIVE_TYPE")
            .takes_value(true)
            .help("Snapshot archive format to use."),
    )
    .arg(
        Arg::with_name("snapshot_zstd_compression_level")
            .long("snapshot-zstd-compression-level")
            .default_value(&default_args.snapshot_zstd_compression_level)
            .value_name("LEVEL")
            .takes_value(true)
            .help("The compression level to use when archiving with zstd")
            .long_help(
                "The compression level to use when archiving with zstd. \
                 Higher compression levels generally produce higher \
                 compression ratio at the expense of speed and memory. \
                 See the zstd manpage for more information."
            ),
    )
    .arg(
        Arg::with_name("max_genesis_archive_unpacked_size")
            .long("max-genesis-archive-unpacked-size")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.genesis_archive_unpacked_size)
            .help("maximum total uncompressed file size of downloaded genesis archive"),
    )
    .arg(
        Arg::with_name("wal_recovery_mode")
            .long("wal-recovery-mode")
            .value_name("MODE")
            .takes_value(true)
            .possible_values(&[
                "tolerate_corrupted_tail_records",
                "absolute_consistency",
                "point_in_time",
                "skip_any_corrupted_record",
            ])
            .help("Mode to recovery the ledger db write ahead log."),
    )
    .arg(
        Arg::with_name("poh_pinned_cpu_core")
            .hidden(hidden_unless_forced())
            .long("experimental-poh-pinned-cpu-core")
            .takes_value(true)
            .value_name("CPU_CORE_INDEX")
            .validator(|s| {
                let core_index = usize::from_str(&s).map_err(|e| e.to_string())?;
                let max_index = core_affinity::get_core_ids()
                    .map(|cids| cids.len() - 1)
                    .unwrap_or(0);
                if core_index > max_index {
                    return Err(format!("core index must be in the range [0, {max_index}]"));
                }
                Ok(())
            })
            .help("EXPERIMENTAL: Specify which CPU core PoH is pinned to"),
    )
    .arg(
        Arg::with_name("poh_hashes_per_batch")
            .hidden(hidden_unless_forced())
            .long("poh-hashes-per-batch")
            .takes_value(true)
            .value_name("NUM")
            .help("Specify hashes per batch in PoH service"),
    )
    .arg(
        Arg::with_name("process_ledger_before_services")
            .long("process-ledger-before-services")
            .hidden(hidden_unless_forced())
            .help("Process the local ledger fully before starting networking services"),
    )
    .arg(
        Arg::with_name("account_indexes")
            .long("account-index")
            .takes_value(true)
            .multiple(true)
            .possible_values(&["program-id", "spl-token-owner", "spl-token-mint"])
            .value_name("INDEX")
            .help("Enable an accounts index, indexed by the selected account field"),
    )
    .arg(
        Arg::with_name("account_index_exclude_key")
            .long(EXCLUDE_KEY)
            .takes_value(true)
            .validator(is_pubkey)
            .multiple(true)
            .value_name("KEY")
            .help("When account indexes are enabled, exclude this key from the index."),
    )
    .arg(
        Arg::with_name("account_index_include_key")
            .long(INCLUDE_KEY)
            .takes_value(true)
            .validator(is_pubkey)
            .conflicts_with("account_index_exclude_key")
            .multiple(true)
            .value_name("KEY")
            .help(
                "When account indexes are enabled, only include specific keys in the index. \
                 This overrides --account-index-exclude-key.",
            ),
    )
    .arg(
        Arg::with_name("accounts_db_verify_refcounts")
            .long("accounts-db-verify-refcounts")
            .help(
                "Debug option to scan all append vecs and verify account index refcounts \
                 prior to clean",
            )
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_scan_filter_for_shrinking")
            .long("accounts-db-scan-filter-for-shrinking")
            .takes_value(true)
            .possible_values(&["all", "only-abnormal", "only-abnormal-with-verify"])
            .help(
                "Debug option to use different type of filtering for accounts index scan in \
                shrinking. \"all\" will scan both in-memory and on-disk accounts index, which is the default. \
                \"only-abnormal\" will scan in-memory accounts index only for abnormal entries and \
                skip scanning on-disk accounts index by assuming that on-disk accounts index contains \
                only normal accounts index entry. \"only-abnormal-with-verify\" is similar to \
                \"only-abnormal\", which will scan in-memory index for abnormal entries, but will also \
                verify that on-disk account entries are indeed normal.",
            )
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_test_skip_rewrites")
            .long("accounts-db-test-skip-rewrites")
            .help(
                "Debug option to skip rewrites for rent-exempt accounts but still add them in \
                 bank delta hash calculation",
            )
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("no_skip_initial_accounts_db_clean")
            .long("no-skip-initial-accounts-db-clean")
            .help("Do not skip the initial cleaning of accounts when verifying snapshot bank")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_squash_storages_method")
            .long("accounts-db-squash-storages-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(&["pack", "append"])
            .help("Squash multiple account storage files together using this method")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_access_storages_method")
            .long("accounts-db-access-storages-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(&["mmap", "file"])
            .help("Access account storages using this method")
    )
    .arg(
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
    )
    .arg(
        Arg::with_name("accounts_db_ancient_storage_ideal_size")
            .long("accounts-db-ancient-storage-ideal-size")
            .value_name("BYTES")
            .validator(is_parsable::<u64>)
            .takes_value(true)
            .help("The smallest size of ideal ancient storage.")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_max_ancient_storages")
            .long("accounts-db-max-ancient-storages")
            .value_name("USIZE")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help("The number of ancient storages the ancient slot combining should converge to.")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_hash_calculation_pubkey_bins")
            .long("accounts-db-hash-calculation-pubkey-bins")
            .value_name("USIZE")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help("The number of pubkey bins used for accounts hash calculation.")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_cache_limit_mb")
            .long("accounts-db-cache-limit-mb")
            .value_name("MEGABYTES")
            .validator(is_parsable::<u64>)
            .takes_value(true)
            .help(
                "How large the write cache for account data can become. If this is exceeded, \
                 the cache is flushed more aggressively.",
            ),
    )
    .arg(
        Arg::with_name("accounts_db_read_cache_limit_mb")
            .long("accounts-db-read-cache-limit-mb")
            .value_name("MAX | LOW,HIGH")
            .takes_value(true)
            .min_values(1)
            .max_values(2)
            .multiple(false)
            .require_delimiter(true)
            .help("How large the read cache for account data can become, in mebibytes")
            .long_help(
                "How large the read cache for account data can become, in mebibytes. \
                 If given a single value, it will be the maximum size for the cache. \
                 If given a pair of values, they will be the low and high watermarks \
                 for the cache. When the cache exceeds the high watermark, entries will \
                 be evicted until the size reaches the low watermark."
            )
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_experimental_accumulator_hash")
            .long("accounts-db-experimental-accumulator-hash")
            .help("Enables the experimental accumulator hash")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_verify_experimental_accumulator_hash")
            .long("accounts-db-verify-experimental-accumulator-hash")
            .help("Verifies the experimental accumulator hash")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_snapshots_use_experimental_accumulator_hash")
            .long("accounts-db-snapshots-use-experimental-accumulator-hash")
            .help("Snapshots use the experimental accumulator hash")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_index_scan_results_limit_mb")
            .long("accounts-index-scan-results-limit-mb")
            .value_name("MEGABYTES")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help(
                "How large accumulated results from an accounts index scan can become. If \
                 this is exceeded, the scan aborts.",
            ),
    )
    .arg(
        Arg::with_name("accounts_index_bins")
            .long("accounts-index-bins")
            .value_name("BINS")
            .validator(is_pow2)
            .takes_value(true)
            .help("Number of bins to divide the accounts index into"),
    )
    .arg(
        Arg::with_name("accounts_index_path")
            .long("accounts-index-path")
            .value_name("PATH")
            .takes_value(true)
            .multiple(true)
            .help(
                "Persistent accounts-index location. \
                May be specified multiple times. \
                [default: <LEDGER>/accounts_index]",
            ),
    )
    .arg(
        Arg::with_name("accounts_db_test_hash_calculation")
            .long("accounts-db-test-hash-calculation")
            .help(
                "Enables testing of hash calculation using stores in AccountsHashVerifier. \
                 This has a computational cost.",
            ),
    )
    .arg(
        Arg::with_name("accounts_shrink_optimize_total_space")
            .long("accounts-shrink-optimize-total-space")
            .takes_value(true)
            .value_name("BOOLEAN")
            .default_value(&default_args.accounts_shrink_optimize_total_space)
            .help(
                "When this is set to true, the system will shrink the most sparse accounts \
                 and when the overall shrink ratio is above the specified \
                 accounts-shrink-ratio, the shrink will stop and it will skip all other less \
                 sparse accounts.",
            ),
    )
    .arg(
        Arg::with_name("accounts_shrink_ratio")
            .long("accounts-shrink-ratio")
            .takes_value(true)
            .value_name("RATIO")
            .default_value(&default_args.accounts_shrink_ratio)
            .help(
                "Specifies the shrink ratio for the accounts to be shrunk. The shrink ratio \
                 is defined as the ratio of the bytes alive over the  total bytes used. If \
                 the account's shrink ratio is less than this ratio it becomes a candidate \
                 for shrinking. The value must between 0. and 1.0 inclusive.",
            ),
    )
    .arg(
        Arg::with_name("allow_private_addr")
            .long("allow-private-addr")
            .takes_value(false)
            .help("Allow contacting private ip addresses")
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("log_messages_bytes_limit")
            .long("log-messages-bytes-limit")
            .takes_value(true)
            .validator(is_parsable::<usize>)
            .value_name("BYTES")
            .help("Maximum number of bytes written to the program log before truncation"),
    )
    .arg(
        Arg::with_name("banking_trace_dir_byte_limit")
            // expose friendly alternative name to cli than internal
            // implementation-oriented one
            .long("enable-banking-trace")
            .value_name("BYTES")
            .validator(is_parsable::<DirByteLimit>)
            .takes_value(true)
            // Firstly, zero limit value causes tracer to be disabled
            // altogether, intuitively. On the other hand, this non-zero
            // default doesn't enable banking tracer unless this flag is
            // explicitly given, similar to --limit-ledger-size.
            // see configure_banking_trace_dir_byte_limit() for this.
            .default_value(&default_args.banking_trace_dir_byte_limit)
            .help(
                "Enables the banking trace explicitly, which is enabled by default and writes \
                 trace files for simulate-leader-blocks, retaining up to the default or \
                 specified total bytes in the ledger. This flag can be used to override its \
                 byte limit.",
            ),
    )
    .arg(
        Arg::with_name("disable_banking_trace")
            .long("disable-banking-trace")
            .conflicts_with("banking_trace_dir_byte_limit")
            .takes_value(false)
            .help("Disables the banking trace"),
    )
    .arg(
        Arg::with_name("delay_leader_block_for_pending_fork")
            .hidden(hidden_unless_forced())
            .long("delay-leader-block-for-pending-fork")
            .takes_value(false)
            .help(
                "Delay leader block creation while replaying a block which descends from the \
                current fork and has a lower slot than our next leader slot. If we don't \
                delay here, our new leader block will be on a different fork from the \
                block we are replaying and there is a high chance that the cluster will \
                confirm that block's fork rather than our leader block's fork because it \
                was created before we started creating ours.",
            ),
    )
    .arg(
        Arg::with_name("block_verification_method")
            .long("block-verification-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(BlockVerificationMethod::cli_names())
            .help(BlockVerificationMethod::cli_message()),
    )
    .arg(
        Arg::with_name("block_production_method")
            .long("block-production-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(BlockProductionMethod::cli_names())
            .help(BlockProductionMethod::cli_message()),
    )
    .arg(
        Arg::with_name("transaction_struct")
            .long("transaction-structure")
            .value_name("STRUCT")
            .takes_value(true)
            .possible_values(TransactionStructure::cli_names())
            .help(TransactionStructure::cli_message()),
    )
    .arg(
        Arg::with_name("unified_scheduler_handler_threads")
            .long("unified-scheduler-handler-threads")
            .value_name("COUNT")
            .takes_value(true)
            .validator(|s| is_within_range(s, 1..))
            .help(DefaultSchedulerPool::cli_message()),
    )
    .arg(
        Arg::with_name("wen_restart")
            .long("wen-restart")
            .hidden(hidden_unless_forced())
            .value_name("FILE")
            .takes_value(true)
            .required(false)
            .conflicts_with("wait_for_supermajority")
            .requires("wen_restart_coordinator")
            .help(
                "Only used during coordinated cluster restarts.\
                \n\n\
                Need to also specify the leader's pubkey in --wen-restart-leader.\
                \n\n\
                When specified, the validator will enter Wen Restart mode which \
                pauses normal activity. Validators in this mode will gossip their last \
                vote to reach consensus on a safe restart slot and repair all blocks \
                on the selected fork. The safe slot will be a descendant of the latest \
                optimistically confirmed slot to ensure we do not roll back any \
                optimistically confirmed slots. \
                \n\n\
                The progress in this mode will be saved in the file location provided. \
                If consensus is reached, the validator will automatically exit with 200 \
                status code. Then the operators are expected to restart the validator \
                with --wait_for_supermajority and other arguments (including new shred_version, \
                supermajority slot, and bankhash) given in the error log before the exit so \
                the cluster will resume execution. The progress file will be kept around \
                for future debugging. \
                \n\n\
                If wen_restart fails, refer to the progress file (in proto3 format) for \
                further debugging and watch the discord channel for instructions.",
            ),
    )
    .arg(
        Arg::with_name("wen_restart_coordinator")
            .long("wen-restart-coordinator")
            .hidden(hidden_unless_forced())
            .value_name("PUBKEY")
            .takes_value(true)
            .required(false)
            .requires("wen_restart")
            .help(
                "Specifies the pubkey of the leader used in wen restart. \
                May get stuck if the leader used is different from others.",
            ),
    )
}
