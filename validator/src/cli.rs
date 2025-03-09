use {
    crate::commands,
    clap::{crate_description, crate_name, App, AppSettings, Arg, ArgMatches, SubCommand},
    log::warn,
    solana_accounts_db::{
        accounts_db::{
            DEFAULT_ACCOUNTS_SHRINK_OPTIMIZE_TOTAL_SPACE, DEFAULT_ACCOUNTS_SHRINK_RATIO,
        },
        hardened_unpack::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    },
    solana_clap_utils::{
        hidden_unless_forced,
        input_validators::{
            is_parsable, is_pubkey, is_pubkey_or_keypair, is_slot, is_url_or_moniker,
        },
    },
    solana_core::banking_trace::BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT,
    solana_faucet::faucet::{self, FAUCET_PORT},
    solana_net_utils::{MINIMUM_VALIDATOR_PORT_RANGE_WIDTH, VALIDATOR_PORT_RANGE},
    solana_rayon_threadlimit::get_thread_count,
    solana_rpc::{rpc::MAX_REQUEST_BODY_SIZE, rpc_pubsub_service::PubSubConfig},
    solana_rpc_client_api::request::{DELINQUENT_VALIDATOR_SLOT_DISTANCE, MAX_MULTIPLE_ACCOUNTS},
    solana_runtime::{
        snapshot_bank_utils::{
            DEFAULT_FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS,
            DEFAULT_INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS,
        },
        snapshot_utils::{
            SnapshotVersion, DEFAULT_ARCHIVE_COMPRESSION,
            DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
        },
    },
    solana_sdk::{
        clock::Slot, epoch_schedule::MINIMUM_SLOTS_PER_EPOCH, hash::Hash, quic::QUIC_PORT_OFFSET,
        rpc_port,
    },
    solana_send_transaction_service::send_transaction_service::{self},
    solana_streamer::quic::{
        DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER,
        DEFAULT_MAX_STAKED_CONNECTIONS, DEFAULT_MAX_STREAMS_PER_MS,
        DEFAULT_MAX_UNSTAKED_CONNECTIONS, DEFAULT_QUIC_ENDPOINTS,
    },
    solana_tpu_client::tpu_client::{DEFAULT_TPU_CONNECTION_POOL_SIZE, DEFAULT_VOTE_USE_QUIC},
    std::{path::PathBuf, str::FromStr},
};

pub mod thread_args;
use thread_args::{thread_args, DefaultThreadArgs};

// The default minimal snapshot download speed (bytes/second)
const DEFAULT_MIN_SNAPSHOT_DOWNLOAD_SPEED: u64 = 10485760;
// The maximum times of snapshot download abort and retry
const MAX_SNAPSHOT_DOWNLOAD_ABORT: u32 = 5;
// We've observed missed leader slots leading to deadlocks on test validator
// with less than 2 ticks per slot.
const MINIMUM_TICKS_PER_SLOT: u64 = 2;

pub fn app<'a>(version: &'a str, default_args: &'a DefaultArgs) -> App<'a, 'a> {
    let app = App::new(crate_name!())
        .about(crate_description!())
        .version(version)
        .global_setting(AppSettings::ColoredHelp)
        .global_setting(AppSettings::InferSubcommands)
        .global_setting(AppSettings::UnifiedHelpMessage)
        .global_setting(AppSettings::VersionlessSubcommands)
        .subcommand(commands::exit::command(default_args))
        .subcommand(commands::authorized_voter::command(default_args))
        .subcommand(commands::contact_info::command(default_args))
        .subcommand(commands::repair_shred_from_peer::command(default_args))
        .subcommand(commands::repair_whitelist::command(default_args))
        .subcommand(
            SubCommand::with_name("init").about("Initialize the ledger directory then exit"),
        )
        .subcommand(commands::monitor::command(default_args))
        .subcommand(SubCommand::with_name("run").about("Run the validator"))
        .subcommand(commands::plugin::command(default_args))
        .subcommand(commands::set_identity::command())
        .subcommand(commands::set_log_filter::command())
        .subcommand(commands::staked_nodes_overrides::command())
        .subcommand(commands::wait_for_restart_window::command())
        .subcommand(commands::set_public_address::command());

    commands::run::add_args(app, default_args)
        .args(&thread_args(&default_args.thread_args))
        .args(&get_deprecated_arguments())
        .after_help("The default subcommand is run")
}

/// Deprecated argument description should be moved into the [`deprecated_arguments()`] function,
/// expressed as an instance of this type.
struct DeprecatedArg {
    /// Deprecated argument description, moved here as is.
    ///
    /// `hidden` property will be modified by [`deprecated_arguments()`] to only show this argument
    /// if [`hidden_unless_forced()`] says they should be displayed.
    arg: Arg<'static, 'static>,

    /// If simply replaced by a different argument, this is the name of the replacement.
    ///
    /// Content should be an argument name, as presented to users.
    replaced_by: Option<&'static str>,

    /// An explanation to be shown to the user if they still use this argument.
    ///
    /// Content should be a complete sentence or several, ending with a period.
    usage_warning: Option<&'static str>,
}

fn deprecated_arguments() -> Vec<DeprecatedArg> {
    let mut res = vec![];

    // This macro reduces indentation and removes some noise from the argument declaration list.
    macro_rules! add_arg {
        (
            $arg:expr
            $( , replaced_by: $replaced_by:expr )?
            $( , usage_warning: $usage_warning:expr )?
            $(,)?
        ) => {
            let replaced_by = add_arg!(@into-option $( $replaced_by )?);
            let usage_warning = add_arg!(@into-option $( $usage_warning )?);
            res.push(DeprecatedArg {
                arg: $arg,
                replaced_by,
                usage_warning,
            });
        };

        (@into-option) => { None };
        (@into-option $v:expr) => { Some($v) };
    }

    // deprecated in v2.1 by PR #2721
    add_arg!(Arg::with_name("accounts_index_memory_limit_mb")
        .long("accounts-index-memory-limit-mb")
        .value_name("MEGABYTES")
        .validator(is_parsable::<usize>)
        .takes_value(true)
        .help(
            "How much memory the accounts index can consume. If this is exceeded, some \
         account index entries will be stored on disk.",
        ),
        usage_warning: "index memory limit has been deprecated. The limit arg has no effect now.",
    );
    add_arg!(Arg::with_name("accountsdb_repl_bind_address")
        .long("accountsdb-repl-bind-address")
        .value_name("HOST")
        .takes_value(true)
        .validator(solana_net_utils::is_host)
        .help(
            "IP address to bind the AccountsDb Replication port [default: use \
                     --bind-address]",
        ));
    add_arg!(Arg::with_name("accountsdb_repl_port")
        .long("accountsdb-repl-port")
        .value_name("PORT")
        .takes_value(true)
        .validator(port_validator)
        .help("Enable AccountsDb Replication Service on this port"));
    add_arg!(Arg::with_name("accountsdb_repl_threads")
        .long("accountsdb-repl-threads")
        .value_name("NUMBER")
        .validator(is_parsable::<usize>)
        .takes_value(true)
        .help("Number of threads to use for servicing AccountsDb Replication requests"));
    add_arg!(Arg::with_name("disable_accounts_disk_index")
        .long("disable-accounts-disk-index")
        .help("Disable the disk-based accounts index if it is enabled by default.")
        .conflicts_with("accounts_index_memory_limit_mb"));
    add_arg!(
        Arg::with_name("disable_quic_servers")
            .long("disable-quic-servers")
            .takes_value(false),
        usage_warning: "The quic server cannot be disabled.",
    );
    add_arg!(Arg::with_name("enable_accountsdb_repl")
        .long("enable-accountsdb-repl")
        .takes_value(false)
        .help("Enable AccountsDb Replication"));
    add_arg!(
        Arg::with_name("enable_cpi_and_log_storage")
            .long("enable-cpi-and-log-storage")
            .requires("enable_rpc_transaction_history")
            .takes_value(false)
            .help(
                "Include CPI inner instructions, logs and return data in the historical \
                 transaction info stored",
            ),
        replaced_by: "enable-extended-tx-metadata-storage",
    );
    add_arg!(
        Arg::with_name("enable_quic_servers")
            .long("enable-quic-servers"),
        usage_warning: "The quic server is now enabled by default.",
    );
    // All etcd config is deprecated as of v2.2
    add_arg!(Arg::with_name("etcd_cacert_file")
        .long("etcd-cacert-file")
        .required_if("tower_storage", "etcd")
        .value_name("FILE")
        .takes_value(true)
        .help("verify the TLS certificate of the etcd endpoint using this CA bundle"),);
    add_arg!(Arg::with_name("etcd_cert_file")
        .long("etcd-cert-file")
        .required_if("tower_storage", "etcd")
        .value_name("FILE")
        .takes_value(true)
        .help("TLS certificate to use when establishing a connection to the etcd endpoint"),);
    add_arg!(Arg::with_name("etcd_domain_name")
        .long("etcd-domain-name")
        .required_if("tower_storage", "etcd")
        .value_name("DOMAIN")
        .default_value("localhost")
        .takes_value(true)
        .help("domain name against which to verify the etcd server’s TLS certificate"),);
    add_arg!(Arg::with_name("etcd_endpoint")
        .long("etcd-endpoint")
        .required_if("tower_storage", "etcd")
        .value_name("HOST:PORT")
        .takes_value(true)
        .multiple(true)
        .validator(solana_net_utils::is_host_port)
        .help("etcd gRPC endpoint to connect with"),);
    add_arg!(Arg::with_name("etcd_key_file")
        .long("etcd-key-file")
        .required_if("tower_storage", "etcd")
        .value_name("FILE")
        .takes_value(true)
        .help("TLS key file to use when establishing a connection to the etcd endpoint"),);

    add_arg!(Arg::with_name("minimal_rpc_api")
        .long("minimal-rpc-api")
        .takes_value(false)
        .help("Only expose the RPC methods required to serve snapshots to other nodes"));
    add_arg!(
        Arg::with_name("no_check_vote_account")
            .long("no-check-vote-account")
            .takes_value(false)
            .conflicts_with("no_voting")
            .requires("entrypoint")
            .help("Skip the RPC vote account sanity check"),
        usage_warning: "Vote account sanity checks are no longer performed by default.",
    );
    add_arg!(Arg::with_name("no_rocksdb_compaction")
        .long("no-rocksdb-compaction")
        .takes_value(false)
        .help("Disable manual compaction of the ledger database"));
    add_arg!(
        Arg::with_name("replay_slots_concurrently")
            .long("replay-slots-concurrently")
            .help("Allow concurrent replay of slots on different forks")
            .conflicts_with("replay_forks_threads"),
        replaced_by: "replay_forks_threads",
        usage_warning: "Equivalent behavior to this flag would be --replay-forks-threads 4");
    add_arg!(Arg::with_name("rocksdb_compaction_interval")
        .long("rocksdb-compaction-interval-slots")
        .value_name("ROCKSDB_COMPACTION_INTERVAL_SLOTS")
        .takes_value(true)
        .help("Number of slots between compacting ledger"));
    // Deprecated in v2.2
    add_arg!(Arg::with_name("rocksdb_fifo_shred_storage_size")
        .long("rocksdb-fifo-shred-storage-size")
        .value_name("SHRED_STORAGE_SIZE_BYTES")
        .takes_value(true)
        .validator(is_parsable::<u64>)
        .help(
            "The shred storage size in bytes. The suggested value is at least 50% of your ledger \
             storage size. If this argument is unspecified, we will assign a proper value based \
             on --limit-ledger-size. If --limit-ledger-size is not presented, it means there is \
             no limitation on the ledger size and thus rocksdb_fifo_shred_storage_size will also \
             be unbounded.",
        ));
    add_arg!(Arg::with_name("rocksdb_max_compaction_jitter")
        .long("rocksdb-max-compaction-jitter-slots")
        .value_name("ROCKSDB_MAX_COMPACTION_JITTER_SLOTS")
        .takes_value(true)
        .help("Introduce jitter into the compaction to offset compaction operation"));
    add_arg!(Arg::with_name("rpc_pubsub_max_connections")
        .long("rpc-pubsub-max-connections")
        .value_name("NUMBER")
        .takes_value(true)
        .validator(is_parsable::<usize>)
        .help(
            "The maximum number of connections that RPC PubSub will support. This is a \
             hard limit and no new connections beyond this limit can be made until an old \
             connection is dropped."
        ));
    add_arg!(Arg::with_name("rpc_pubsub_max_fragment_size")
        .long("rpc-pubsub-max-fragment-size")
        .value_name("BYTES")
        .takes_value(true)
        .validator(is_parsable::<usize>)
        .help(
            "The maximum length in bytes of acceptable incoming frames. Messages longer \
             than this will be rejected"
        ));
    add_arg!(Arg::with_name("rpc_pubsub_max_in_buffer_capacity")
        .long("rpc-pubsub-max-in-buffer-capacity")
        .value_name("BYTES")
        .takes_value(true)
        .validator(is_parsable::<usize>)
        .help("The maximum size in bytes to which the incoming websocket buffer can grow."));
    add_arg!(Arg::with_name("rpc_pubsub_max_out_buffer_capacity")
        .long("rpc-pubsub-max-out-buffer-capacity")
        .value_name("BYTES")
        .takes_value(true)
        .validator(is_parsable::<usize>)
        .help("The maximum size in bytes to which the outgoing websocket buffer can grow."));
    add_arg!(
        Arg::with_name("skip_poh_verify")
            .long("skip-poh-verify")
            .takes_value(false)
            .help("Skip ledger verification at validator bootup."),
        replaced_by: "skip-startup-ledger-verification",
    );
    // Deprecated as of v2.2
    add_arg!(
        Arg::with_name("tower_storage")
            .long("tower-storage")
            .possible_values(&["file", "etcd"])
            .default_value("file")
            .takes_value(true)
            .help("Where to store the tower"),
        usage_warning: "\"etcd\" is no longer supported, and the functionality from setting \
            \"file\" will be become the sole behavior",
    );

    res
}

// Helper to add arguments that are no longer used but are being kept around to avoid breaking
// validator startup commands.
fn get_deprecated_arguments() -> Vec<Arg<'static, 'static>> {
    deprecated_arguments()
        .into_iter()
        .map(|info| {
            let arg = info.arg;
            // Hide all deprecated arguments by default.
            arg.hidden(hidden_unless_forced())
        })
        .collect()
}

pub fn warn_for_deprecated_arguments(matches: &ArgMatches) {
    for DeprecatedArg {
        arg,
        replaced_by,
        usage_warning,
    } in deprecated_arguments().into_iter()
    {
        if matches.is_present(arg.b.name) {
            let mut msg = format!("--{} is deprecated", arg.b.name.replace('_', "-"));
            if let Some(replaced_by) = replaced_by {
                msg.push_str(&format!(", please use --{replaced_by}"));
            }
            msg.push('.');
            if let Some(usage_warning) = usage_warning {
                msg.push_str(&format!("  {usage_warning}"));
                if !msg.ends_with('.') {
                    msg.push('.');
                }
            }
            warn!("{}", msg);
        }
    }
}

pub struct DefaultArgs {
    pub bind_address: String,
    pub dynamic_port_range: String,
    pub ledger_path: String,

    pub genesis_archive_unpacked_size: String,
    pub health_check_slot_distance: String,
    pub tower_storage: String,
    pub etcd_domain_name: String,
    pub send_transaction_service_config: send_transaction_service::Config,

    pub rpc_max_multiple_accounts: String,
    pub rpc_pubsub_max_active_subscriptions: String,
    pub rpc_pubsub_queue_capacity_items: String,
    pub rpc_pubsub_queue_capacity_bytes: String,
    pub rpc_send_transaction_retry_ms: String,
    pub rpc_send_transaction_batch_ms: String,
    pub rpc_send_transaction_leader_forward_count: String,
    pub rpc_send_transaction_service_max_retries: String,
    pub rpc_send_transaction_batch_size: String,
    pub rpc_send_transaction_retry_pool_max_size: String,
    pub rpc_threads: String,
    pub rpc_blocking_threads: String,
    pub rpc_niceness_adjustment: String,
    pub rpc_bigtable_timeout: String,
    pub rpc_bigtable_instance_name: String,
    pub rpc_bigtable_app_profile_id: String,
    pub rpc_bigtable_max_message_size: String,
    pub rpc_max_request_body_size: String,
    pub rpc_pubsub_worker_threads: String,
    pub rpc_pubsub_notification_threads: String,

    pub maximum_local_snapshot_age: String,
    pub maximum_full_snapshot_archives_to_retain: String,
    pub maximum_incremental_snapshot_archives_to_retain: String,
    pub snapshot_packager_niceness_adjustment: String,
    pub full_snapshot_archive_interval_slots: String,
    pub incremental_snapshot_archive_interval_slots: String,
    pub min_snapshot_download_speed: String,
    pub max_snapshot_download_abort: String,

    pub contact_debug_interval: String,

    pub snapshot_version: SnapshotVersion,
    pub snapshot_archive_format: String,
    pub snapshot_zstd_compression_level: String,

    pub rocksdb_shred_compaction: String,
    pub rocksdb_ledger_compression: String,
    pub rocksdb_perf_sample_interval: String,

    pub accounts_shrink_optimize_total_space: String,
    pub accounts_shrink_ratio: String,
    pub tpu_connection_pool_size: String,

    pub tpu_max_connections_per_peer: String,
    pub tpu_max_connections_per_ipaddr_per_minute: String,
    pub tpu_max_staked_connections: String,
    pub tpu_max_unstaked_connections: String,
    pub tpu_max_fwd_staked_connections: String,
    pub tpu_max_fwd_unstaked_connections: String,
    pub tpu_max_streams_per_ms: String,

    pub num_quic_endpoints: String,
    pub vote_use_quic: String,

    // Exit subcommand
    pub exit_min_idle_time: String,
    pub exit_max_delinquent_stake: String,

    pub banking_trace_dir_byte_limit: String,

    pub wen_restart_path: String,

    pub thread_args: DefaultThreadArgs,
}

impl DefaultArgs {
    pub fn new() -> Self {
        let default_send_transaction_service_config = send_transaction_service::Config::default();

        DefaultArgs {
            bind_address: "0.0.0.0".to_string(),
            ledger_path: "ledger".to_string(),
            dynamic_port_range: format!("{}-{}", VALIDATOR_PORT_RANGE.0, VALIDATOR_PORT_RANGE.1),
            maximum_local_snapshot_age: "2500".to_string(),
            genesis_archive_unpacked_size: MAX_GENESIS_ARCHIVE_UNPACKED_SIZE.to_string(),
            rpc_max_multiple_accounts: MAX_MULTIPLE_ACCOUNTS.to_string(),
            health_check_slot_distance: DELINQUENT_VALIDATOR_SLOT_DISTANCE.to_string(),
            tower_storage: "file".to_string(),
            etcd_domain_name: "localhost".to_string(),
            rpc_pubsub_max_active_subscriptions: PubSubConfig::default()
                .max_active_subscriptions
                .to_string(),
            rpc_pubsub_queue_capacity_items: PubSubConfig::default()
                .queue_capacity_items
                .to_string(),
            rpc_pubsub_queue_capacity_bytes: PubSubConfig::default()
                .queue_capacity_bytes
                .to_string(),
            send_transaction_service_config: send_transaction_service::Config::default(),
            rpc_send_transaction_retry_ms: default_send_transaction_service_config
                .retry_rate_ms
                .to_string(),
            rpc_send_transaction_batch_ms: default_send_transaction_service_config
                .batch_send_rate_ms
                .to_string(),
            rpc_send_transaction_leader_forward_count: default_send_transaction_service_config
                .leader_forward_count
                .to_string(),
            rpc_send_transaction_service_max_retries: default_send_transaction_service_config
                .service_max_retries
                .to_string(),
            rpc_send_transaction_batch_size: default_send_transaction_service_config
                .batch_size
                .to_string(),
            rpc_send_transaction_retry_pool_max_size: default_send_transaction_service_config
                .retry_pool_max_size
                .to_string(),
            rpc_threads: num_cpus::get().to_string(),
            rpc_blocking_threads: 1.max(num_cpus::get() / 4).to_string(),
            rpc_niceness_adjustment: "0".to_string(),
            rpc_bigtable_timeout: "30".to_string(),
            rpc_bigtable_instance_name: solana_storage_bigtable::DEFAULT_INSTANCE_NAME.to_string(),
            rpc_bigtable_app_profile_id: solana_storage_bigtable::DEFAULT_APP_PROFILE_ID
                .to_string(),
            rpc_bigtable_max_message_size: solana_storage_bigtable::DEFAULT_MAX_MESSAGE_SIZE
                .to_string(),
            rpc_pubsub_worker_threads: "4".to_string(),
            rpc_pubsub_notification_threads: get_thread_count().to_string(),
            maximum_full_snapshot_archives_to_retain: DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN
                .to_string(),
            maximum_incremental_snapshot_archives_to_retain:
                DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN.to_string(),
            snapshot_packager_niceness_adjustment: "0".to_string(),
            full_snapshot_archive_interval_slots: DEFAULT_FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS
                .to_string(),
            incremental_snapshot_archive_interval_slots:
                DEFAULT_INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS.to_string(),
            min_snapshot_download_speed: DEFAULT_MIN_SNAPSHOT_DOWNLOAD_SPEED.to_string(),
            max_snapshot_download_abort: MAX_SNAPSHOT_DOWNLOAD_ABORT.to_string(),
            snapshot_archive_format: DEFAULT_ARCHIVE_COMPRESSION.to_string(),
            snapshot_zstd_compression_level: "1".to_string(), // level 1 is optimized for speed
            contact_debug_interval: "120000".to_string(),
            snapshot_version: SnapshotVersion::default(),
            rocksdb_shred_compaction: "level".to_string(),
            rocksdb_ledger_compression: "none".to_string(),
            rocksdb_perf_sample_interval: "0".to_string(),
            accounts_shrink_optimize_total_space: DEFAULT_ACCOUNTS_SHRINK_OPTIMIZE_TOTAL_SPACE
                .to_string(),
            accounts_shrink_ratio: DEFAULT_ACCOUNTS_SHRINK_RATIO.to_string(),
            tpu_connection_pool_size: DEFAULT_TPU_CONNECTION_POOL_SIZE.to_string(),
            tpu_max_connections_per_ipaddr_per_minute:
                DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE.to_string(),
            vote_use_quic: DEFAULT_VOTE_USE_QUIC.to_string(),
            tpu_max_connections_per_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER.to_string(),
            tpu_max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS.to_string(),
            tpu_max_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS.to_string(),
            tpu_max_fwd_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS
                .saturating_add(DEFAULT_MAX_UNSTAKED_CONNECTIONS)
                .to_string(),
            tpu_max_fwd_unstaked_connections: 0.to_string(),
            tpu_max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS.to_string(),
            num_quic_endpoints: DEFAULT_QUIC_ENDPOINTS.to_string(),
            rpc_max_request_body_size: MAX_REQUEST_BODY_SIZE.to_string(),
            exit_min_idle_time: "10".to_string(),
            exit_max_delinquent_stake: "5".to_string(),
            banking_trace_dir_byte_limit: BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT.to_string(),
            wen_restart_path: "wen_restart_progress.proto".to_string(),
            thread_args: DefaultThreadArgs::default(),
        }
    }
}

impl Default for DefaultArgs {
    fn default() -> Self {
        Self::new()
    }
}

pub fn port_validator(port: String) -> Result<(), String> {
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

pub fn port_range_validator(port_range: String) -> Result<(), String> {
    if let Some((start, end)) = solana_net_utils::parse_port_range(&port_range) {
        if end - start < MINIMUM_VALIDATOR_PORT_RANGE_WIDTH {
            Err(format!(
                "Port range is too small.  Try --dynamic-port-range {}-{}",
                start,
                start + MINIMUM_VALIDATOR_PORT_RANGE_WIDTH
            ))
        } else if end.checked_add(QUIC_PORT_OFFSET).is_none() {
            Err("Invalid dynamic_port_range.".to_string())
        } else {
            Ok(())
        }
    } else {
        Err("Invalid port range".to_string())
    }
}

pub(crate) fn hash_validator(hash: String) -> Result<(), String> {
    Hash::from_str(&hash)
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

/// Test validator
pub fn test_app<'a>(version: &'a str, default_args: &'a DefaultTestArgs) -> App<'a, 'a> {
    App::new("solana-test-validator")
        .about("Test Validator")
        .version(version)
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("json_rpc_url")
                .short("u")
                .long("url")
                .value_name("URL_OR_MONIKER")
                .takes_value(true)
                .validator(is_url_or_moniker)
                .help(
                    "URL for Solana's JSON RPC or moniker (or their first letter): \
                     [mainnet-beta, testnet, devnet, localhost]",
                ),
        )
        .arg(
            Arg::with_name("mint_address")
                .long("mint")
                .value_name("PUBKEY")
                .validator(is_pubkey)
                .takes_value(true)
                .help(
                    "Address of the mint account that will receive tokens created at genesis. If \
                     the ledger already exists then this parameter is silently ignored \
                     [default: client keypair]",
                ),
        )
        .arg(
            Arg::with_name("ledger_path")
                .short("l")
                .long("ledger")
                .value_name("DIR")
                .takes_value(true)
                .required(true)
                .default_value("test-ledger")
                .help("Use DIR as ledger location"),
        )
        .arg(
            Arg::with_name("reset")
                .short("r")
                .long("reset")
                .takes_value(false)
                .help(
                    "Reset the ledger to genesis if it exists. By default the validator will \
                     resume an existing ledger (if present)",
                ),
        )
        .arg(
            Arg::with_name("quiet")
                .short("q")
                .long("quiet")
                .takes_value(false)
                .conflicts_with("log")
                .help("Quiet mode: suppress normal output"),
        )
        .arg(
            Arg::with_name("log")
                .long("log")
                .takes_value(false)
                .conflicts_with("quiet")
                .help("Log mode: stream the validator log"),
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
            Arg::with_name("faucet_port")
                .long("faucet-port")
                .value_name("PORT")
                .takes_value(true)
                .default_value(&default_args.faucet_port)
                .validator(port_validator)
                .help("Enable the faucet on this port"),
        )
        .arg(
            Arg::with_name("rpc_port")
                .long("rpc-port")
                .value_name("PORT")
                .takes_value(true)
                .default_value(&default_args.rpc_port)
                .validator(port_validator)
                .help("Enable JSON RPC on this port, and the next port for the RPC websocket"),
        )
        .arg(
            Arg::with_name("enable_rpc_bigtable_ledger_storage")
                .long("enable-rpc-bigtable-ledger-storage")
                .takes_value(false)
                .hidden(hidden_unless_forced())
                .help(
                    "Fetch historical transaction info from a BigTable instance as a fallback to \
                     local ledger data",
                ),
        )
        .arg(
            Arg::with_name("enable_bigtable_ledger_upload")
                .long("enable-bigtable-ledger-upload")
                .takes_value(false)
                .hidden(hidden_unless_forced())
                .help("Upload new confirmed blocks into a BigTable instance"),
        )
        .arg(
            Arg::with_name("rpc_bigtable_instance")
                .long("rpc-bigtable-instance")
                .value_name("INSTANCE_NAME")
                .takes_value(true)
                .hidden(hidden_unless_forced())
                .default_value("solana-ledger")
                .help("Name of BigTable instance to target"),
        )
        .arg(
            Arg::with_name("rpc_bigtable_app_profile_id")
                .long("rpc-bigtable-app-profile-id")
                .value_name("APP_PROFILE_ID")
                .takes_value(true)
                .hidden(hidden_unless_forced())
                .default_value(solana_storage_bigtable::DEFAULT_APP_PROFILE_ID)
                .help("Application profile id to use in Bigtable requests"),
        )
        .arg(
            Arg::with_name("rpc_pubsub_enable_vote_subscription")
                .long("rpc-pubsub-enable-vote-subscription")
                .takes_value(false)
                .help("Enable the unstable RPC PubSub `voteSubscribe` subscription"),
        )
        .arg(
            Arg::with_name("rpc_pubsub_enable_block_subscription")
                .long("rpc-pubsub-enable-block-subscription")
                .takes_value(false)
                .help("Enable the unstable RPC PubSub `blockSubscribe` subscription"),
        )
        .arg(
            Arg::with_name("bpf_program")
                .long("bpf-program")
                .value_names(&["ADDRESS_OR_KEYPAIR", "SBF_PROGRAM.SO"])
                .takes_value(true)
                .number_of_values(2)
                .multiple(true)
                .help(
                    "Add a SBF program to the genesis configuration with upgrades disabled. If \
                     the ledger already exists then this parameter is silently ignored. The first \
                     argument can be a pubkey string or path to a keypair",
                ),
        )
        .arg(
            Arg::with_name("upgradeable_program")
                .long("upgradeable-program")
                .value_names(&["ADDRESS_OR_KEYPAIR", "SBF_PROGRAM.SO", "UPGRADE_AUTHORITY"])
                .takes_value(true)
                .number_of_values(3)
                .multiple(true)
                .help(
                    "Add an upgradeable SBF program to the genesis configuration. If the ledger \
                     already exists then this parameter is silently ignored. First and third \
                     arguments can be a pubkey string or path to a keypair. Upgrade authority set \
                     to \"none\" disables upgrades",
                ),
        )
        .arg(
            Arg::with_name("account")
                .long("account")
                .value_names(&["ADDRESS", "DUMP.JSON"])
                .takes_value(true)
                .number_of_values(2)
                .allow_hyphen_values(true)
                .multiple(true)
                .help(
                    "Load an account from the provided JSON file (see `solana account --help` on \
                     how to dump an account to file). Files are searched for relatively to CWD \
                     and tests/fixtures. If ADDRESS is omitted via the `-` placeholder, the one \
                     in the file will be used. If the ledger already exists then this parameter \
                     is silently ignored",
                ),
        )
        .arg(
            Arg::with_name("account_dir")
                .long("account-dir")
                .value_name("DIRECTORY")
                .validator(|value| {
                    value
                        .parse::<PathBuf>()
                        .map_err(|err| format!("error parsing '{value}': {err}"))
                        .and_then(|path| {
                            if path.exists() && path.is_dir() {
                                Ok(())
                            } else {
                                Err(format!(
                                    "path does not exist or is not a directory: {value}"
                                ))
                            }
                        })
                })
                .takes_value(true)
                .multiple(true)
                .help(
                    "Load all the accounts from the JSON files found in the specified DIRECTORY \
                     (see also the `--account` flag). If the ledger already exists then this \
                     parameter is silently ignored",
                ),
        )
        .arg(
            Arg::with_name("ticks_per_slot")
                .long("ticks-per-slot")
                .value_name("TICKS")
                .validator(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|err| format!("error parsing '{value}': {err}"))
                        .and_then(|ticks| {
                            if ticks < MINIMUM_TICKS_PER_SLOT {
                                Err(format!("value must be >= {MINIMUM_TICKS_PER_SLOT}"))
                            } else {
                                Ok(())
                            }
                        })
                })
                .takes_value(true)
                .help("The number of ticks in a slot"),
        )
        .arg(
            Arg::with_name("slots_per_epoch")
                .long("slots-per-epoch")
                .value_name("SLOTS")
                .validator(|value| {
                    value
                        .parse::<Slot>()
                        .map_err(|err| format!("error parsing '{value}': {err}"))
                        .and_then(|slot| {
                            if slot < MINIMUM_SLOTS_PER_EPOCH {
                                Err(format!("value must be >= {MINIMUM_SLOTS_PER_EPOCH}"))
                            } else {
                                Ok(())
                            }
                        })
                })
                .takes_value(true)
                .help(
                    "Override the number of slots in an epoch. If the ledger already exists then \
                     this parameter is silently ignored",
                ),
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
                     [default: 127.0.0.1]",
                ),
        )
        .arg(
            Arg::with_name("dynamic_port_range")
                .long("dynamic-port-range")
                .value_name("MIN_PORT-MAX_PORT")
                .takes_value(true)
                .validator(port_range_validator)
                .help("Range to use for dynamically assigned ports [default: 1024-65535]"),
        )
        .arg(
            Arg::with_name("bind_address")
                .long("bind-address")
                .value_name("HOST")
                .takes_value(true)
                .validator(solana_net_utils::is_host)
                .default_value("0.0.0.0")
                .help("IP address to bind the validator ports [default: 0.0.0.0]"),
        )
        .arg(
            Arg::with_name("clone_account")
                .long("clone")
                .short("c")
                .value_name("ADDRESS")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .multiple(true)
                .requires("json_rpc_url")
                .help(
                    "Copy an account from the cluster referenced by the --url argument the \
                     genesis configuration. If the ledger already exists then this parameter is \
                     silently ignored",
                ),
        )
        .arg(
            Arg::with_name("maybe_clone_account")
                .long("maybe-clone")
                .value_name("ADDRESS")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .multiple(true)
                .requires("json_rpc_url")
                .help(
                    "Copy an account from the cluster referenced by the --url argument, skipping \
                     it if it doesn't exist. If the ledger already exists then this parameter is \
                     silently ignored",
                ),
        )
        .arg(
            Arg::with_name("clone_upgradeable_program")
                .long("clone-upgradeable-program")
                .value_name("ADDRESS")
                .takes_value(true)
                .validator(is_pubkey_or_keypair)
                .multiple(true)
                .requires("json_rpc_url")
                .help(
                    "Copy an upgradeable program and its executable data from the cluster \
                     referenced by the --url argument the genesis configuration. If the ledger \
                     already exists then this parameter is silently ignored",
                ),
        )
        .arg(
            Arg::with_name("warp_slot")
                .required(false)
                .long("warp-slot")
                .short("w")
                .takes_value(true)
                .value_name("WARP_SLOT")
                .validator(is_slot)
                .min_values(0)
                .max_values(1)
                .help(
                    "Warp the ledger to WARP_SLOT after starting the validator. If no slot is \
                     provided then the current slot of the cluster referenced by the --url \
                     argument will be used",
                ),
        )
        .arg(
            Arg::with_name("limit_ledger_size")
                .long("limit-ledger-size")
                .value_name("SHRED_COUNT")
                .takes_value(true)
                .default_value(default_args.limit_ledger_size.as_str())
                .help("Keep this amount of shreds in root slots."),
        )
        .arg(
            Arg::with_name("faucet_sol")
                .long("faucet-sol")
                .takes_value(true)
                .value_name("SOL")
                .default_value(default_args.faucet_sol.as_str())
                .help(
                    "Give the faucet address this much SOL in genesis. If the ledger already \
                     exists then this parameter is silently ignored",
                ),
        )
        .arg(
            Arg::with_name("faucet_time_slice_secs")
                .long("faucet-time-slice-secs")
                .takes_value(true)
                .value_name("SECS")
                .default_value(default_args.faucet_time_slice_secs.as_str())
                .help("Time slice (in secs) over which to limit faucet requests"),
        )
        .arg(
            Arg::with_name("faucet_per_time_sol_cap")
                .long("faucet-per-time-sol-cap")
                .takes_value(true)
                .value_name("SOL")
                .min_values(0)
                .max_values(1)
                .help("Per-time slice limit for faucet requests, in SOL"),
        )
        .arg(
            Arg::with_name("faucet_per_request_sol_cap")
                .long("faucet-per-request-sol-cap")
                .takes_value(true)
                .value_name("SOL")
                .min_values(0)
                .max_values(1)
                .help("Per-request limit for faucet requests, in SOL"),
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
            Arg::with_name("deactivate_feature")
                .long("deactivate-feature")
                .takes_value(true)
                .value_name("FEATURE_PUBKEY")
                .validator(is_pubkey)
                .multiple(true)
                .help("deactivate this feature in genesis."),
        )
        .arg(
            Arg::with_name("compute_unit_limit")
                .long("compute-unit-limit")
                .alias("max-compute-units")
                .value_name("COMPUTE_UNITS")
                .validator(is_parsable::<u64>)
                .takes_value(true)
                .help("Override the runtime's compute unit limit per transaction"),
        )
        .arg(
            Arg::with_name("log_messages_bytes_limit")
                .long("log-messages-bytes-limit")
                .value_name("BYTES")
                .validator(is_parsable::<usize>)
                .takes_value(true)
                .help("Maximum number of bytes written to the program log before truncation"),
        )
        .arg(
            Arg::with_name("transaction_account_lock_limit")
                .long("transaction-account-lock-limit")
                .value_name("NUM_ACCOUNTS")
                .validator(is_parsable::<u64>)
                .takes_value(true)
                .help("Override the runtime's account lock limit per transaction"),
        )
        .arg(
            Arg::with_name("clone_feature_set")
                .long("clone-feature-set")
                .takes_value(false)
                .requires("json_rpc_url")
                .help(
                    "Copy a feature set from the cluster referenced by the --url \
                     argument in the genesis configuration. If the ledger \
                     already exists then this parameter is silently ignored",
                ),
        )
}

pub struct DefaultTestArgs {
    pub rpc_port: String,
    pub faucet_port: String,
    pub limit_ledger_size: String,
    pub faucet_sol: String,
    pub faucet_time_slice_secs: String,
}

impl DefaultTestArgs {
    pub fn new() -> Self {
        DefaultTestArgs {
            rpc_port: rpc_port::DEFAULT_RPC_PORT.to_string(),
            faucet_port: FAUCET_PORT.to_string(),
            /* 10,000 was derived empirically by watching the size
             * of the rocksdb/ directory self-limit itself to the
             * 40MB-150MB range when running `solana-test-validator`
             */
            limit_ledger_size: 10_000.to_string(),
            faucet_sol: (1_000_000.).to_string(),
            faucet_time_slice_secs: (faucet::TIME_SLICE).to_string(),
        }
    }
}

impl Default for DefaultTestArgs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn make_sure_deprecated_arguments_are_sorted_alphabetically() {
        let deprecated = deprecated_arguments();

        for i in 0..deprecated.len().saturating_sub(1) {
            let curr_name = deprecated[i].arg.b.name;
            let next_name = deprecated[i + 1].arg.b.name;

            assert!(
                curr_name != next_name,
                "Arguments in `deprecated_arguments()` should be distinct.\nArguments {} and {} \
                 use the same name: {}",
                i,
                i + 1,
                curr_name,
            );

            assert!(
                curr_name < next_name,
                "To generate better diffs and for readability purposes, `deprecated_arguments()` \
                 should list arguments in alphabetical order.\nArguments {} and {} are \
                 not.\nArgument {} name: {}\nArgument {} name: {}",
                i,
                i + 1,
                i,
                curr_name,
                i + 1,
                next_name,
            );
        }
    }
}
