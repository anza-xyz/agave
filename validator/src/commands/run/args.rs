use {
    crate::{
        bootstrap::RpcBootstrapConfig,
        cli::{hash_validator, port_range_validator, port_validator, DefaultArgs},
        commands::{FromClapArgMatches, Result},
    },
    agave_snapshots::{SnapshotVersion, SUPPORTED_ARCHIVE_COMPRESSION},
    clap::{values_t, App, Arg, ArgMatches},
    solana_accounts_db::utils::create_and_canonicalize_directory,
    solana_clap_utils::{
        hidden_unless_forced,
        input_parsers::keypair_of,
        input_validators::{
            is_keypair_or_ask_keyword, is_non_zero, is_parsable, is_pow2, is_pubkey,
            is_pubkey_or_keypair, is_slot, is_within_range, validate_cpu_ranges,
            validate_maximum_full_snapshot_archives_to_retain,
            validate_maximum_incremental_snapshot_archives_to_retain,
        },
        keypair::SKIP_SEED_PHRASE_VALIDATION_ARG,
    },
    solana_core::{
        banking_trace::DirByteLimit,
        validator::{BlockProductionMethod, BlockVerificationMethod},
    },
    solana_keypair::Keypair,
    solana_ledger::{blockstore_options::BlockstoreOptions, use_snapshot_archives_at_startup},
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_rpc::{rpc::JsonRpcConfig, rpc_pubsub_service::PubSubConfig},
    solana_send_transaction_service::send_transaction_service::Config as SendTransactionServiceConfig,
    solana_signer::Signer,
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    std::{collections::HashSet, net::SocketAddr, path::PathBuf, str::FromStr},
};

const EXCLUDE_KEY: &str = "account-index-exclude-key";
const INCLUDE_KEY: &str = "account-index-include-key";

// Declared out of line to allow use of #[rustfmt::skip]
#[rustfmt::skip]
const WEN_RESTART_HELP: &str =
    "Only used during coordinated cluster restarts.\n\n\
     Need to also specify the leader's pubkey in --wen-restart-leader.\n\n\
     When specified, the validator will enter Wen Restart mode which pauses normal activity. \
     Validators in this mode will gossip their last vote to reach consensus on a safe restart \
     slot and repair all blocks on the selected fork. The safe slot will be a descendant of the \
     latest optimistically confirmed slot to ensure we do not roll back any optimistically \
     confirmed slots.\n\n\
     The progress in this mode will be saved in the file location provided. If consensus is \
     reached, the validator will automatically exit with 200 status code. Then the operators are \
     expected to restart the validator with --wait_for_supermajority and other arguments \
     (including new shred_version, supermajority slot, and bankhash) given in the error log \
     before the exit so the cluster will resume execution. The progress file will be kept around \
     for future debugging.\n\n\
     If wen_restart fails, refer to the progress file (in proto3 format) for further debugging and \
     watch the discord channel for instructions.";

pub mod account_secondary_indexes;
pub mod blockstore_options;
pub mod json_rpc_config;
pub mod pub_sub_config;
pub mod rpc_bigtable_config;
pub mod rpc_bootstrap_config;
pub mod send_transaction_config;

#[derive(Debug, PartialEq)]
pub struct RunArgs {
    pub identity_keypair: Keypair,
    pub ledger_path: PathBuf,
    pub logfile: Option<PathBuf>,
    pub entrypoints: Vec<SocketAddr>,
    pub known_validators: Option<HashSet<Pubkey>>,
    pub socket_addr_space: SocketAddrSpace,
    pub rpc_bootstrap_config: RpcBootstrapConfig,
    pub blockstore_options: BlockstoreOptions,
    pub json_rpc_config: JsonRpcConfig,
    pub pub_sub_config: PubSubConfig,
    pub send_transaction_service_config: SendTransactionServiceConfig,
}

impl FromClapArgMatches for RunArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        let identity_keypair =
            keypair_of(matches, "identity").ok_or(clap::Error::with_description(
                "The --identity <KEYPAIR> argument is required",
                clap::ErrorKind::ArgumentNotFound,
            ))?;

        let ledger_path = PathBuf::from(matches.value_of("ledger_path").ok_or(
            clap::Error::with_description(
                "The --ledger <DIR> argument is required",
                clap::ErrorKind::ArgumentNotFound,
            ),
        )?);
        // Canonicalize ledger path to avoid issues with symlink creation
        let ledger_path =
            create_and_canonicalize_directory(ledger_path.as_path()).map_err(|err| {
                crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                    "failed to create and canonicalize ledger path '{}': {err}",
                    ledger_path.display(),
                )))
            })?;

        let logfile = matches
            .value_of("logfile")
            .map(String::from)
            .unwrap_or_else(|| format!("agave-validator-{}.log", identity_keypair.pubkey()));
        let logfile = if logfile == "-" {
            None
        } else {
            Some(PathBuf::from(logfile))
        };

        let mut entrypoints = values_t!(matches, "entrypoint", String).unwrap_or_default();
        // sort() + dedup() to yield a vector of unique elements
        entrypoints.sort();
        entrypoints.dedup();
        let entrypoints = entrypoints
            .into_iter()
            .map(|entrypoint| {
                solana_net_utils::parse_host_port(&entrypoint).map_err(|err| {
                    crate::commands::Error::Dynamic(Box::<dyn std::error::Error>::from(format!(
                        "failed to parse entrypoint address: {err}"
                    )))
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let known_validators = validators_set(
            &identity_keypair.pubkey(),
            matches,
            "known_validators",
            "known validator",
        )?;

        let socket_addr_space = SocketAddrSpace::new(matches.is_present("allow_private_addr"));

        Ok(RunArgs {
            identity_keypair,
            ledger_path,
            logfile,
            entrypoints,
            known_validators,
            socket_addr_space,
            rpc_bootstrap_config: RpcBootstrapConfig::from_clap_arg_match(matches)?,
            blockstore_options: BlockstoreOptions::from_clap_arg_match(matches)?,
            json_rpc_config: JsonRpcConfig::from_clap_arg_match(matches)?,
            pub_sub_config: PubSubConfig::from_clap_arg_match(matches)?,
            send_transaction_service_config: SendTransactionServiceConfig::from_clap_arg_match(
                matches,
            )?,
        })
    }
}

pub fn add_args<'a>(app: App<'a, 'a>, default_args: &'a DefaultArgs) -> App<'a, 'a> {
    app.arg(
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
                "Include an additional authorized voter keypair. May be specified multiple times. \
                 [default: the --identity keypair]",
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
                "Validator vote account public key. If unspecified, voting will be disabled. The \
                 authorized voter for the account must either be the --identity keypair or set by \
                 the --authorized-voter argument",
            ),
    )
    .arg(
        Arg::with_name("init_complete_file")
            .long("init-complete-file")
            .value_name("FILE")
            .takes_value(true)
            .help(
                "Create this file if it doesn't already exist once validator initialization is \
                 complete",
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
        Arg::with_name("no_voting")
            .long("no-voting")
            .takes_value(false)
            .help("Launch validator without voting"),
    )
    .arg(
        Arg::with_name("restricted_repair_only_mode")
            .long("restricted-repair-only-mode")
            .takes_value(false)
            .help(
                "Do not publish the Gossip, TPU, TVU or Repair Service ports. Doing so causes the \
                 node to operate in a limited capacity that reduces its exposure to the rest of \
                 the cluster. The --no-voting flag is implicit when this flag is enabled",
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
        Arg::with_name("account_paths")
            .long("accounts")
            .value_name("PATHS")
            .takes_value(true)
            .multiple(true)
            .help(
                "Comma separated persistent accounts location. May be specified multiple times. \
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
        Arg::with_name("snapshots")
            .long("snapshots")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as the base location for snapshots.")
            .long_help(
                "Use DIR as the base location for snapshots. Snapshot archives will use DIR \
                 unless --full-snapshot-archive-path or --incremental-snapshot-archive-path is \
                 specified. Additionally, a subdirectory named \"snapshots\" will be created in \
                 DIR. This subdirectory holds internal files/data that are used when generating \
                 snapshot archives. [default: --ledger value]",
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
            .help("Use DIR as full snapshot archives location [default: --snapshots value]"),
    )
    .arg(
        Arg::with_name("incremental_snapshot_archive_path")
            .long("incremental-snapshot-archive-path")
            .conflicts_with("no-incremental-snapshots")
            .value_name("DIR")
            .takes_value(true)
            .help("Use DIR as incremental snapshot archives location [default: --snapshots value]"),
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
        Arg::with_name("public_tpu_addr")
            .long("public-tpu-address")
            .alias("tpu-host-addr")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help(
                "Specify TPU QUIC address to advertise in gossip [default: ask --entrypoint or \
                 localhost when --entrypoint is not provided]",
            ),
    )
    .arg(
        Arg::with_name("public_tpu_forwards_addr")
            .long("public-tpu-forwards-address")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help(
                "Specify TPU Forwards QUIC address to advertise in gossip [default: ask \
                 --entrypoint or localhostwhen --entrypoint is not provided]",
            ),
    )
    .arg(
        Arg::with_name("public_tvu_addr")
            .long("public-tvu-address")
            .alias("tvu-host-addr")
            .value_name("HOST:PORT")
            .takes_value(true)
            .validator(solana_net_utils::is_host_port)
            .help(
                "Specify TVU address to advertise in gossip [default: ask --entrypoint or \
                 localhost when --entrypoint is not provided]",
            ),
    )
    .arg(
        Arg::with_name("tpu_vortexor_receiver_address")
            .long("tpu-vortexor-receiver-address")
            .value_name("HOST:PORT")
            .takes_value(true)
            .hidden(hidden_unless_forced())
            .validator(solana_net_utils::is_host_port)
            .help(
                "TPU Vortexor Receiver address to which verified transaction packet will be \
                 forwarded.",
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
            .conflicts_with_all(&[
                "no_incremental_snapshots",
                "snapshot_interval_slots",
                "full_snapshot_interval_slots",
            ])
            .help("Disable all snapshot generation"),
    )
    .arg(
        Arg::with_name("snapshot_interval_slots")
            .long("snapshot-interval-slots")
            .alias("incremental-snapshot-interval-slots")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.incremental_snapshot_archive_interval_slots)
            .validator(is_non_zero)
            .help("Number of slots between generating snapshots")
            .long_help(
                "Number of slots between generating snapshots. If incremental snapshots are \
                 enabled, this sets the incremental snapshot interval. If incremental snapshots \
                 are disabled, this sets the full snapshot interval. Must be greater than zero.",
            ),
    )
    .arg(
        Arg::with_name("full_snapshot_interval_slots")
            .long("full-snapshot-interval-slots")
            .value_name("NUMBER")
            .takes_value(true)
            .default_value(&default_args.full_snapshot_archive_interval_slots)
            .validator(is_non_zero)
            .help("Number of slots between generating full snapshots")
            .long_help(
                "Number of slots between generating full snapshots. Only used when incremental \
                 snapshots are enabled. Must be greater than the incremental snapshot interval. \
                 Must be greater than zero.",
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
                "The maximum number of full snapshot archives to hold on to when purging older \
                 snapshots.",
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
                "The maximum number of incremental snapshot archives to hold on to when purging \
                 older snapshots.",
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
                "Add this value to niceness of snapshot packager thread. Negative value increases \
                 priority, positive value decreases priority.",
            ),
    )
    .arg(
        Arg::with_name("minimal_snapshot_download_speed")
            .long("minimal-snapshot-download-speed")
            .value_name("MINIMAL_SNAPSHOT_DOWNLOAD_SPEED")
            .takes_value(true)
            .default_value(&default_args.min_snapshot_download_speed)
            .help(
                "The minimal speed of snapshot downloads measured in bytes/second. If the initial \
                 download speed falls below this threshold, the system will retry the download \
                 against a different rpc node.",
            ),
    )
    .arg(
        Arg::with_name("maximum_snapshot_download_abort")
            .long("maximum-snapshot-download-abort")
            .value_name("MAXIMUM_SNAPSHOT_DOWNLOAD_ABORT")
            .takes_value(true)
            .default_value(&default_args.max_snapshot_download_abort)
            .help(
                "The maximum number of times to abort and retry when encountering a slow snapshot \
                 download.",
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
        Arg::with_name("skip_startup_ledger_verification")
            .long("skip-startup-ledger-verification")
            .takes_value(false)
            .help("Skip ledger verification at validator bootup."),
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
                 production until it sees a vote land in a rooted slot. This prevents double \
                 signing. Turn off to risk double signing a block.",
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
                "A snapshot hash must be published in gossip by this validator to be accepted. \
                 May be specified multiple times. If unspecified any snapshot hash will be \
                 accepted",
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
                "A list of validators to prioritize repairs from. If specified, repair requests \
                 from validators in the list will be prioritized over requests from other \
                 validators. [default: all validators]",
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
                "A list of validators to gossip with. If specified, gossip will not push/pull \
                 from from validators outside this set. [default: all validators]",
            ),
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
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help(
                "Controls the max concurrent connections per IpAddr or staked identity.Overrides \
                 tpu-max-connections-per-unstaked-peer and tpu-max-connections-per-staked-peer",
            ),
    )
    .arg(
        Arg::with_name("tpu_max_connections_per_unstaked_peer")
            .long("tpu-max-connections-per-unstaked-peer")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_unstaked_peer)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections per IpAddr for unstaked clients."),
    )
    .arg(
        Arg::with_name("tpu_max_connections_per_staked_peer")
            .long("tpu-max-connections-per-staked-peer")
            .takes_value(true)
            .default_value(&default_args.tpu_max_connections_per_staked_peer)
            .validator(is_parsable::<u32>)
            .hidden(hidden_unless_forced())
            .help("Controls the max concurrent connections per staked identity."),
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
            .help(
                "The number of QUIC endpoints used for TPU and TPU-Forward. It can be increased \
                 to increase network ingest throughput, at the expense of higher CPU and general \
                 validator load.",
            ),
    )
    .arg(
        Arg::with_name("staked_nodes_overrides")
            .long("staked-nodes-overrides")
            .value_name("PATH")
            .takes_value(true)
            .help(
                "Provide path to a yaml file with custom overrides for stakes of specific \
                 identities. Overriding the amount of stake this validator considers as valid for \
                 other peers in network. The stake amount is used for calculating the number of \
                 QUIC streams permitted from the peer and vote packet sender stage. Format of the \
                 file: `staked_map_id: {<pubkey>: <SOL stake amount>}",
            ),
    )
    .arg(
        Arg::with_name("bind_address")
            .long("bind-address")
            .value_name("HOST")
            .takes_value(true)
            .validator(solana_net_utils::is_host)
            .default_value(&default_args.bind_address)
            .multiple(true)
            .help(
                "Repeatable. IP addresses to bind the validator ports on. First is primary (used \
                 on startup), the rest may be switched to during operation.",
            ),
    )
    .arg(
        Arg::with_name("rpc_bind_address")
            .long("rpc-bind-address")
            .value_name("HOST")
            .takes_value(true)
            .validator(solana_net_utils::is_host)
            .help(
                "IP address to bind the RPC port [default: 127.0.0.1 if --private-rpc is present, \
                 otherwise use --bind-address]",
            ),
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
                "The compression level to use when archiving with zstd. Higher compression levels \
                 generally produce higher compression ratio at the expense of speed and memory. \
                 See the zstd manpage for more information.",
            ),
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
                "When account indexes are enabled, only include specific keys in the index. This \
                 overrides --account-index-exclude-key.",
            ),
    )
    .arg(
        Arg::with_name("accounts_db_verify_refcounts")
            .long("accounts-db-verify-refcounts")
            .help(
                "Debug option to scan all append vecs and verify account index refcounts prior to \
                 clean",
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
                 shrinking. \"all\" will scan both in-memory and on-disk accounts index, which is \
                 the default. \"only-abnormal\" will scan in-memory accounts index only for \
                 abnormal entries and skip scanning on-disk accounts index by assuming that \
                 on-disk accounts index contains only normal accounts index entry. \
                 \"only-abnormal-with-verify\" is similar to \"only-abnormal\", which will scan \
                 in-memory index for abnormal entries, but will also verify that on-disk account \
                 entries are indeed normal.",
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
        Arg::with_name("accounts_db_access_storages_method")
            .long("accounts-db-access-storages-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(&["mmap", "file"])
            .help("Access account storages using this method"),
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
        Arg::with_name("accounts_db_cache_limit_mb")
            .long("accounts-db-cache-limit-mb")
            .value_name("MEGABYTES")
            .validator(is_parsable::<u64>)
            .takes_value(true)
            .help(
                "How large the write cache for account data can become. If this is exceeded, the \
                 cache is flushed more aggressively.",
            ),
    )
    .arg(
        Arg::with_name("accounts_db_read_cache_limit")
            .long("accounts-db-read-cache-limit")
            .value_name("LOW,HIGH")
            .takes_value(true)
            .min_values(2)
            .max_values(2)
            .multiple(false)
            .require_delimiter(true)
            .help("How large the read cache for account data can become, in bytes")
            .long_help(
                "How large the read cache for account data can become, in bytes. The values will \
                 be the low and high watermarks for the cache. When the cache exceeds the high \
                 watermark, entries will be evicted until the size reaches the low watermark.",
            )
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_db_mark_obsolete_accounts")
            .long("accounts-db-mark-obsolete-accounts")
            .help("Controls obsolete account tracking")
            .takes_value(true)
            .possible_values(&["enabled", "disabled"])
            .long_help(
                "Controls obsolete account tracking. This feature tracks obsolete accounts in the \
                 account storage entry allowing for earlier cleaning of obsolete accounts in the \
                 storages and index. This value is currently enabled by default.",
            )
            .hidden(hidden_unless_forced()),
    )
    .arg(
        Arg::with_name("accounts_index_scan_results_limit_mb")
            .long("accounts-index-scan-results-limit-mb")
            .value_name("MEGABYTES")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help(
                "How large accumulated results from an accounts index scan can become. If this is \
                 exceeded, the scan aborts.",
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
        Arg::with_name("accounts_index_initial_accounts_count")
            .long("accounts-index-initial-accounts-count")
            .value_name("NUMBER")
            .validator(is_parsable::<usize>)
            .takes_value(true)
            .help("Pre-allocate the accounts index, assuming this many accounts")
            .hidden(hidden_unless_forced()),
    )
    .arg(
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
    )
    .arg(
        Arg::with_name("enable_accounts_disk_index")
            .long("enable-accounts-disk-index")
            .help("Enables the disk-based accounts index")
            .long_help(
                "Enables the disk-based accounts index. Reduce the memory footprint of the \
                 accounts index at the cost of index performance.",
            ),
    )
    .arg(
        Arg::with_name("accounts_shrink_optimize_total_space")
            .long("accounts-shrink-optimize-total-space")
            .takes_value(true)
            .value_name("BOOLEAN")
            .default_value(&default_args.accounts_shrink_optimize_total_space)
            .help(
                "When this is set to true, the system will shrink the most sparse accounts and \
                 when the overall shrink ratio is above the specified accounts-shrink-ratio, the \
                 shrink will stop and it will skip all other less sparse accounts.",
            ),
    )
    .arg(
        Arg::with_name("accounts_shrink_ratio")
            .long("accounts-shrink-ratio")
            .takes_value(true)
            .value_name("RATIO")
            .default_value(&default_args.accounts_shrink_ratio)
            .help(
                "Specifies the shrink ratio for the accounts to be shrunk. The shrink ratio is \
                 defined as the ratio of the bytes alive over the  total bytes used. If the \
                 account's shrink ratio is less than this ratio it becomes a candidate for \
                 shrinking. The value must between 0. and 1.0 inclusive.",
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
                 trace files for simulate-leader-blocks, retaining up to the default or specified \
                 total bytes in the ledger. This flag can be used to override its byte limit.",
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
                 current fork and has a lower slot than our next leader slot. If we don't delay \
                 here, our new leader block will be on a different fork from the block we are \
                 replaying and there is a high chance that the cluster will confirm that block's \
                 fork rather than our leader block's fork because it was created before we \
                 started creating ours.",
            ),
    )
    .arg(
        Arg::with_name("block_verification_method")
            .long("block-verification-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(BlockVerificationMethod::cli_names())
            .default_value(BlockVerificationMethod::default().into())
            .help(BlockVerificationMethod::cli_message()),
    )
    .arg(
        Arg::with_name("block_production_method")
            .long("block-production-method")
            .value_name("METHOD")
            .takes_value(true)
            .possible_values(BlockProductionMethod::cli_names())
            .default_value(BlockProductionMethod::default().into())
            .help(BlockProductionMethod::cli_message()),
    )
    .arg(
        Arg::with_name("block_production_pacing_fill_time_millis")
            .long("block-production-pacing-fill-time-millis")
            .value_name("MILLIS")
            .takes_value(true)
            .default_value(&default_args.block_production_pacing_fill_time_millis)
            .help(
                "Pacing fill time in milliseconds for the central-scheduler block production \
                 method",
            ),
    )
    .arg(
        Arg::with_name("enable_scheduler_bindings")
            .long("enable-scheduler-bindings")
            .takes_value(false)
            .help("Enables external processes to connect and manage block production"),
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
            .help(WEN_RESTART_HELP),
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
                "Specifies the pubkey of the leader used in wen restart. May get stuck if the \
                 leader used is different from others.",
            ),
    )
    .arg(
        Arg::with_name("retransmit_xdp_interface")
            .hidden(hidden_unless_forced())
            .long("experimental-retransmit-xdp-interface")
            .takes_value(true)
            .value_name("INTERFACE")
            .requires("retransmit_xdp_cpu_cores")
            .help("EXPERIMENTAL: The network interface to use for XDP retransmit"),
    )
    .arg(
        Arg::with_name("retransmit_xdp_cpu_cores")
            .hidden(hidden_unless_forced())
            .long("experimental-retransmit-xdp-cpu-cores")
            .takes_value(true)
            .value_name("CPU_LIST")
            .validator(|value| {
                validate_cpu_ranges(value, "--experimental-retransmit-xdp-cpu-cores")
            })
            .help("EXPERIMENTAL: Enable XDP retransmit on the specified CPU cores"),
    )
    .arg(
        Arg::with_name("retransmit_xdp_zero_copy")
            .hidden(hidden_unless_forced())
            .long("experimental-retransmit-xdp-zero-copy")
            .takes_value(false)
            .requires("retransmit_xdp_cpu_cores")
            .help("EXPERIMENTAL: Enable XDP zero copy. Requires hardware support"),
    )
    .args(&pub_sub_config::args(/*test_validator:*/ false))
    .args(&json_rpc_config::args())
    .args(&rpc_bigtable_config::args())
    .args(&send_transaction_config::args())
    .args(&rpc_bootstrap_config::args())
    .args(&blockstore_options::args())
}

fn validators_set(
    identity_pubkey: &Pubkey,
    matches: &ArgMatches<'_>,
    matches_name: &str,
    arg_name: &str,
) -> Result<Option<HashSet<Pubkey>>> {
    if matches.is_present(matches_name) {
        let validators_set: Option<HashSet<Pubkey>> = values_t!(matches, matches_name, Pubkey)
            .ok()
            .map(|validators| validators.into_iter().collect());
        if let Some(validators_set) = &validators_set {
            if validators_set.contains(identity_pubkey) {
                return Err(crate::commands::Error::Dynamic(
                    Box::<dyn std::error::Error>::from(format!(
                        "the validator's identity pubkey cannot be a {arg_name}: {identity_pubkey}"
                    )),
                ));
            }
        }
        Ok(validators_set)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::cli::thread_args::thread_args,
        scopeguard::defer,
        std::{
            fs,
            net::{IpAddr, Ipv4Addr},
            path::{absolute, PathBuf},
        },
    };

    impl Default for RunArgs {
        fn default() -> Self {
            let identity_keypair = Keypair::new();
            let ledger_path = absolute(PathBuf::from("ledger")).unwrap();
            let logfile =
                PathBuf::from(format!("agave-validator-{}.log", identity_keypair.pubkey()));
            let entrypoints = vec![];
            let known_validators = None;

            let json_rpc_config =
                crate::commands::run::args::json_rpc_config::tests::default_json_rpc_config();

            RunArgs {
                identity_keypair,
                ledger_path,
                logfile: Some(logfile),
                entrypoints,
                known_validators,
                socket_addr_space: SocketAddrSpace::Global,
                rpc_bootstrap_config: RpcBootstrapConfig::default(),
                blockstore_options: BlockstoreOptions::default(),
                json_rpc_config,
                pub_sub_config: PubSubConfig {
                    worker_threads: 4,
                    notification_threads: None,
                    queue_capacity_items:
                        solana_rpc::rpc_pubsub_service::DEFAULT_QUEUE_CAPACITY_ITEMS,
                    ..PubSubConfig::default_for_tests()
                },
                send_transaction_service_config: SendTransactionServiceConfig::default(),
            }
        }
    }

    impl Clone for RunArgs {
        fn clone(&self) -> Self {
            RunArgs {
                identity_keypair: self.identity_keypair.insecure_clone(),
                logfile: self.logfile.clone(),
                entrypoints: self.entrypoints.clone(),
                known_validators: self.known_validators.clone(),
                socket_addr_space: self.socket_addr_space,
                ledger_path: self.ledger_path.clone(),
                rpc_bootstrap_config: self.rpc_bootstrap_config.clone(),
                blockstore_options: self.blockstore_options.clone(),
                json_rpc_config: self.json_rpc_config.clone(),
                pub_sub_config: self.pub_sub_config.clone(),
                send_transaction_service_config: self.send_transaction_service_config.clone(),
            }
        }
    }

    fn verify_args_struct_by_command(
        default_args: &DefaultArgs,
        args: Vec<&str>,
        expected_args: RunArgs,
    ) {
        let app = add_args(App::new("run_command"), default_args)
            .args(&thread_args(&default_args.thread_args));

        crate::commands::tests::verify_args_struct_by_command::<RunArgs>(
            app,
            [&["run_command"], &args[..]].concat(),
            expected_args,
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_identity() {
        let default_args = DefaultArgs::default();
        let default_run_args = RunArgs::default();

        // generate a keypair
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = default_run_args.identity_keypair.insecure_clone();
        solana_keypair::write_keypair_file(&keypair, &file).unwrap();

        let expected_args = RunArgs {
            identity_keypair: keypair.insecure_clone(),
            ..default_run_args
        };

        // short arg
        {
            verify_args_struct_by_command(
                &default_args,
                vec!["-i", file.to_str().unwrap()],
                expected_args.clone(),
            );
        }

        // long arg
        {
            verify_args_struct_by_command(
                &default_args,
                vec!["--identity", file.to_str().unwrap()],
                expected_args.clone(),
            );
        }
    }

    pub fn verify_args_struct_by_command_run_with_identity_setup(
        default_run_args: RunArgs,
        args: Vec<&str>,
        expected_args: RunArgs,
    ) {
        let default_args = DefaultArgs::default();

        // generate a keypair
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = default_run_args.identity_keypair.insecure_clone();
        solana_keypair::write_keypair_file(&keypair, &file).unwrap();

        let args = [&["--identity", file.to_str().unwrap()], &args[..]].concat();
        verify_args_struct_by_command(&default_args, args, expected_args);
    }

    pub fn verify_args_struct_by_command_run_is_error_with_identity_setup(
        default_run_args: RunArgs,
        args: Vec<&str>,
    ) {
        let default_args = DefaultArgs::default();

        // generate a keypair
        let tmp_dir = tempfile::tempdir().unwrap();
        let file = tmp_dir.path().join("id.json");
        let keypair = default_run_args.identity_keypair.insecure_clone();
        solana_keypair::write_keypair_file(&keypair, &file).unwrap();

        let app = add_args(App::new("run_command"), &default_args)
            .args(&thread_args(&default_args.thread_args));

        crate::commands::tests::verify_args_struct_by_command_is_error::<RunArgs>(
            app,
            [
                &["run_command"],
                &["--identity", file.to_str().unwrap()][..],
                &args[..],
            ]
            .concat(),
        );
    }

    #[test]
    fn verify_args_struct_by_command_run_with_ledger_path() {
        // nonexistent absolute ledger path
        {
            let default_run_args = RunArgs::default();
            let tmp_dir = fs::canonicalize(tempfile::tempdir().unwrap()).unwrap();
            let ledger_path = tmp_dir.join("nonexistent_ledger_path");
            assert!(!fs::exists(&ledger_path).unwrap());

            let expected_args = RunArgs {
                ledger_path: ledger_path.clone(),
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--ledger", ledger_path.to_str().unwrap()],
                expected_args,
            );
            assert!(fs::exists(&ledger_path).unwrap());
        }

        // existing absolute ledger path
        {
            let default_run_args = RunArgs::default();
            let tmp_dir = tempfile::tempdir().unwrap();
            let ledger_path = tmp_dir.path().join("existing_ledger_path");
            fs::create_dir_all(&ledger_path).unwrap();
            let ledger_path = fs::canonicalize(ledger_path).unwrap();
            assert!(fs::exists(ledger_path.as_path()).unwrap());

            let expected_args = RunArgs {
                ledger_path: ledger_path.clone(),
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--ledger", ledger_path.to_str().unwrap()],
                expected_args,
            );
            assert!(fs::exists(&ledger_path).unwrap());
        }

        // nonexistent relative ledger path
        {
            let default_run_args = RunArgs::default();
            let ledger_path = PathBuf::from("nonexistent_ledger_path");
            assert!(!fs::exists(&ledger_path).unwrap());
            defer! {
                fs::remove_dir_all(&ledger_path).unwrap()
            };

            let expected_args = RunArgs {
                ledger_path: absolute(&ledger_path).unwrap(),
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--ledger", ledger_path.to_str().unwrap()],
                expected_args,
            );
            assert!(fs::exists(&ledger_path).unwrap());
        }

        // existing relative ledger path
        {
            let default_run_args = RunArgs::default();
            let ledger_path = PathBuf::from("existing_ledger_path");
            fs::create_dir_all(&ledger_path).unwrap();
            assert!(fs::exists(&ledger_path).unwrap());
            defer! {
                fs::remove_dir_all(&ledger_path).unwrap()
            };

            let expected_args = RunArgs {
                ledger_path: absolute(&ledger_path).unwrap(),
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--ledger", ledger_path.to_str().unwrap()],
                expected_args,
            );
            assert!(fs::exists(&ledger_path).unwrap());
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_log() {
        let default_run_args = RunArgs::default();

        // default
        {
            let expected_args = RunArgs {
                logfile: Some(PathBuf::from(format!(
                    "agave-validator-{}.log",
                    default_run_args.identity_keypair.pubkey()
                ))),
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec![],
                expected_args,
            );
        }

        // short arg
        {
            let expected_args = RunArgs {
                logfile: None,
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec!["-o", "-"],
                expected_args,
            );
        }

        // long arg
        {
            let expected_args = RunArgs {
                logfile: Some(PathBuf::from("custom_log.log")),
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec!["--log", "custom_log.log"],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_entrypoints() {
        // short arg + single entrypoint
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                entrypoints: vec![SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    8000,
                )],
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec!["-n", "127.0.0.1:8000"],
                expected_args,
            );
        }

        // long arg + single entrypoint
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                entrypoints: vec![SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    8000,
                )],
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec!["--entrypoint", "127.0.0.1:8000"],
                expected_args,
            );
        }

        // long arg + multiple entrypoints
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                entrypoints: vec![
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8001),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8002),
                ],
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec![
                    "--entrypoint",
                    "127.0.0.1:8000",
                    "--entrypoint",
                    "127.0.0.1:8001",
                    "--entrypoint",
                    "127.0.0.1:8002",
                ],
                expected_args,
            );
        }

        // long arg + duplicate entrypoints
        {
            let default_run_args = RunArgs::default();
            let expected_args = RunArgs {
                entrypoints: vec![
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8001),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8002),
                ],
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args.clone(),
                vec![
                    "--entrypoint",
                    "127.0.0.1:8000",
                    "--entrypoint",
                    "127.0.0.1:8001",
                    "--entrypoint",
                    "127.0.0.1:8002",
                    "--entrypoint",
                    "127.0.0.1:8000",
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_known_validators() {
        // long arg + single known validator
        {
            let default_run_args = RunArgs::default();
            let known_validators_pubkey = Pubkey::new_unique();
            let known_validators = Some(HashSet::from([known_validators_pubkey]));
            let expected_args = RunArgs {
                known_validators,
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--known-validator", &known_validators_pubkey.to_string()],
                expected_args,
            );
        }

        // alias + single known validator
        {
            let default_run_args = RunArgs::default();
            let known_validators_pubkey = Pubkey::new_unique();
            let known_validators = Some(HashSet::from([known_validators_pubkey]));
            let expected_args = RunArgs {
                known_validators,
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec!["--trusted-validator", &known_validators_pubkey.to_string()],
                expected_args,
            );
        }

        // long arg + multiple known validators
        {
            let default_run_args = RunArgs::default();
            let known_validators_pubkey_1 = Pubkey::new_unique();
            let known_validators_pubkey_2 = Pubkey::new_unique();
            let known_validators_pubkey_3 = Pubkey::new_unique();
            let known_validators = Some(HashSet::from([
                known_validators_pubkey_1,
                known_validators_pubkey_2,
                known_validators_pubkey_3,
            ]));
            let expected_args = RunArgs {
                known_validators,
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--known-validator",
                    &known_validators_pubkey_1.to_string(),
                    "--known-validator",
                    &known_validators_pubkey_2.to_string(),
                    "--known-validator",
                    &known_validators_pubkey_3.to_string(),
                ],
                expected_args,
            );
        }

        // long arg + duplicate known validators
        {
            let default_run_args = RunArgs::default();
            let known_validators_pubkey_1 = Pubkey::new_unique();
            let known_validators_pubkey_2 = Pubkey::new_unique();
            let known_validators = Some(HashSet::from([
                known_validators_pubkey_1,
                known_validators_pubkey_2,
            ]));
            let expected_args = RunArgs {
                known_validators,
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--known-validator",
                    &known_validators_pubkey_1.to_string(),
                    "--known-validator",
                    &known_validators_pubkey_2.to_string(),
                    "--known-validator",
                    &known_validators_pubkey_1.to_string(),
                ],
                expected_args,
            );
        }

        // use identity pubkey as known validator
        {
            let default_args = DefaultArgs::default();
            let default_run_args = RunArgs::default();

            // generate a keypair
            let tmp_dir = tempfile::tempdir().unwrap();
            let file = tmp_dir.path().join("id.json");
            solana_keypair::write_keypair_file(&default_run_args.identity_keypair, &file).unwrap();

            let matches = add_args(App::new("run_command"), &default_args).get_matches_from(vec![
                "run_command",
                "--identity",
                file.to_str().unwrap(),
                "--known-validator",
                &default_run_args.identity_keypair.pubkey().to_string(),
            ]);
            let result = RunArgs::from_clap_arg_match(&matches);
            assert!(result.is_err());
            let error = result.unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "the validator's identity pubkey cannot be a known validator: {}",
                    default_run_args.identity_keypair.pubkey()
                )
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_max_genesis_archive_unpacked_size() {
        // long arg
        {
            let default_run_args = RunArgs::default();
            let max_genesis_archive_unpacked_size = 1000000000;
            let expected_args = RunArgs {
                rpc_bootstrap_config: RpcBootstrapConfig {
                    max_genesis_archive_unpacked_size,
                    ..RpcBootstrapConfig::default()
                },
                ..default_run_args.clone()
            };
            verify_args_struct_by_command_run_with_identity_setup(
                default_run_args,
                vec![
                    "--max-genesis-archive-unpacked-size",
                    &max_genesis_archive_unpacked_size.to_string(),
                ],
                expected_args,
            );
        }
    }

    #[test]
    fn verify_args_struct_by_command_run_with_allow_private_addr() {
        let default_run_args = RunArgs::default();
        let expected_args = RunArgs {
            socket_addr_space: SocketAddrSpace::Unspecified,
            ..default_run_args.clone()
        };
        verify_args_struct_by_command_run_with_identity_setup(
            default_run_args,
            vec!["--allow-private-addr"],
            expected_args,
        );
    }
}
