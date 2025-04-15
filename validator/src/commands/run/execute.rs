use {
    crate::{
        admin_rpc_service::{self, load_staked_nodes_overrides, StakedNodesOverrides},
        bootstrap,
        cli::{self},
        ledger_lockfile, lock_ledger,
    },
    clap::{crate_name, value_t, value_t_or_exit, values_t, values_t_or_exit, ArgMatches},
    crossbeam_channel::unbounded,
    log::*,
    rand::{seq::SliceRandom, thread_rng},
    solana_accounts_db::{
        accounts_db::{AccountShrinkThreshold, AccountsDb, AccountsDbConfig, CreateAncientStorage},
        accounts_file::StorageAccess,
        accounts_index::{
            AccountIndex, AccountSecondaryIndexes, AccountSecondaryIndexesIncludeExclude,
            AccountsIndexConfig, IndexLimitMb, ScanFilter,
        },
        utils::{
            create_all_accounts_run_and_snapshot_dirs, create_and_canonicalize_directories,
            create_and_canonicalize_directory,
        },
    },
    solana_clap_utils::input_parsers::{keypair_of, keypairs_of, pubkey_of, value_of, values_of},
    solana_core::{
        banking_trace::DISABLED_BAKING_TRACE_DIR,
        consensus::tower_storage,
        system_monitor_service::SystemMonitorService,
        tpu::DEFAULT_TPU_COALESCE,
        validator::{
            is_snapshot_config_valid, BlockProductionMethod, BlockVerificationMethod,
            TransactionStructure, Validator, ValidatorConfig, ValidatorError,
            ValidatorStartProgress, ValidatorTpuConfig,
        },
    },
    solana_gossip::{
        cluster_info::{Node, NodeConfig},
        contact_info::ContactInfo,
    },
    solana_ledger::{
        blockstore_cleanup_service::{DEFAULT_MAX_LEDGER_SHREDS, DEFAULT_MIN_MAX_LEDGER_SHREDS},
        blockstore_options::{
            AccessType, BlockstoreCompressionType, BlockstoreOptions, BlockstoreRecoveryMode,
            LedgerColumnOptions,
        },
        use_snapshot_archives_at_startup::{self, UseSnapshotArchivesAtStartup},
    },
    solana_logger::redirect_stderr_to_file,
    solana_perf::recycler::enable_recycler_warming,
    solana_poh::poh_service,
    solana_rpc::{
        rpc::{JsonRpcConfig, RpcBigtableConfig},
        rpc_pubsub_service::PubSubConfig,
    },
    solana_runtime::{
        runtime_config::RuntimeConfig,
        snapshot_bank_utils::DISABLED_SNAPSHOT_ARCHIVE_INTERVAL,
        snapshot_config::{SnapshotConfig, SnapshotUsage},
        snapshot_utils::{self, ArchiveFormat, SnapshotVersion},
    },
    solana_sdk::{
        clock::{Slot, DEFAULT_SLOTS_PER_EPOCH},
        hash::Hash,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    },
    solana_send_transaction_service::send_transaction_service,
    solana_streamer::{quic::QuicServerParams, socket::SocketAddrSpace},
    solana_tpu_client::tpu_client::DEFAULT_TPU_ENABLE_UDP,
    std::{
        collections::HashSet,
        fs::{self, File},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::NonZeroUsize,
        path::{Path, PathBuf},
        process::exit,
        str::FromStr,
        sync::{Arc, RwLock},
        time::Duration,
    },
};

#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    Initialize,
    Run,
}

const MILLIS_PER_SECOND: u64 = 1000;

pub fn execute(
    matches: &ArgMatches,
    solana_version: &str,
    socket_addr_space: SocketAddrSpace,
    ledger_path: &Path,
    operation: Operation,
) -> Result<(), Box<dyn std::error::Error>> {
    let cli::thread_args::NumThreadConfig {
        accounts_db_clean_threads,
        accounts_db_foreground_threads,
        accounts_db_hash_threads,
        accounts_index_flush_threads,
        ip_echo_server_threads,
        rayon_global_threads,
        replay_forks_threads,
        replay_transactions_threads,
        rocksdb_compaction_threads,
        rocksdb_flush_threads,
        tvu_receive_threads,
        tvu_retransmit_threads,
        tvu_sigverify_threads,
    } = cli::thread_args::parse_num_threads_args(matches);

    let identity_keypair = keypair_of(matches, "identity").unwrap_or_else(|| {
        clap::Error::with_description(
            "The --identity <KEYPAIR> argument is required",
            clap::ErrorKind::ArgumentNotFound,
        )
        .exit();
    });

    let logfile = {
        let logfile = matches
            .value_of("logfile")
            .map(|s| s.into())
            .unwrap_or_else(|| format!("agave-validator-{}.log", identity_keypair.pubkey()));

        if logfile == "-" {
            None
        } else {
            println!("log file: {logfile}");
            Some(logfile)
        }
    };
    let use_progress_bar = logfile.is_none();
    let _logger_thread = redirect_stderr_to_file(logfile);

    info!("{} {}", crate_name!(), solana_version);
    info!("Starting validator with: {:#?}", std::env::args_os());

    let cuda = matches.is_present("cuda");
    if cuda {
        solana_perf::perf_libs::init_cuda();
        enable_recycler_warming();
    }

    solana_core::validator::report_target_features();

    let authorized_voter_keypairs = keypairs_of(matches, "authorized_voter_keypairs")
        .map(|keypairs| keypairs.into_iter().map(Arc::new).collect())
        .unwrap_or_else(|| vec![Arc::new(keypair_of(matches, "identity").expect("identity"))]);
    let authorized_voter_keypairs = Arc::new(RwLock::new(authorized_voter_keypairs));

    let staked_nodes_overrides_path = matches
        .value_of("staked_nodes_overrides")
        .map(str::to_string);
    let staked_nodes_overrides = Arc::new(RwLock::new(
        match &staked_nodes_overrides_path {
            None => StakedNodesOverrides::default(),
            Some(p) => load_staked_nodes_overrides(p).unwrap_or_else(|err| {
                error!("Failed to load stake-nodes-overrides from {}: {}", p, err);
                clap::Error::with_description(
                    "Failed to load configuration of stake-nodes-overrides argument",
                    clap::ErrorKind::InvalidValue,
                )
                .exit()
            }),
        }
        .staked_map_id,
    ));

    let init_complete_file = matches.value_of("init_complete_file");

    let rpc_bootstrap_config = bootstrap::RpcBootstrapConfig {
        no_genesis_fetch: matches.is_present("no_genesis_fetch"),
        no_snapshot_fetch: matches.is_present("no_snapshot_fetch"),
        check_vote_account: matches
            .value_of("check_vote_account")
            .map(|url| url.to_string()),
        only_known_rpc: matches.is_present("only_known_rpc"),
        max_genesis_archive_unpacked_size: value_t_or_exit!(
            matches,
            "max_genesis_archive_unpacked_size",
            u64
        ),
        incremental_snapshot_fetch: !matches.is_present("no_incremental_snapshots"),
    };

    let private_rpc = matches.is_present("private_rpc");
    let do_port_check = !matches.is_present("no_port_check");
    let tpu_coalesce = value_t!(matches, "tpu_coalesce_ms", u64)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TPU_COALESCE);

    // Canonicalize ledger path to avoid issues with symlink creation
    let ledger_path = create_and_canonicalize_directory(ledger_path).map_err(|err| {
        format!(
            "unable to access ledger path '{}': {err}",
            ledger_path.display(),
        )
    })?;

    let recovery_mode = matches
        .value_of("wal_recovery_mode")
        .map(BlockstoreRecoveryMode::from);

    let max_ledger_shreds = if matches.is_present("limit_ledger_size") {
        let limit_ledger_size = match matches.value_of("limit_ledger_size") {
            Some(_) => value_t_or_exit!(matches, "limit_ledger_size", u64),
            None => DEFAULT_MAX_LEDGER_SHREDS,
        };
        if limit_ledger_size < DEFAULT_MIN_MAX_LEDGER_SHREDS {
            Err(format!(
                "The provided --limit-ledger-size value was too small, the minimum value is \
                 {DEFAULT_MIN_MAX_LEDGER_SHREDS}"
            ))?;
        }
        Some(limit_ledger_size)
    } else {
        None
    };

    let column_options = LedgerColumnOptions {
        compression_type: match matches.value_of("rocksdb_ledger_compression") {
            None => BlockstoreCompressionType::default(),
            Some(ledger_compression_string) => match ledger_compression_string {
                "none" => BlockstoreCompressionType::None,
                "snappy" => BlockstoreCompressionType::Snappy,
                "lz4" => BlockstoreCompressionType::Lz4,
                "zlib" => BlockstoreCompressionType::Zlib,
                _ => panic!("Unsupported ledger_compression: {ledger_compression_string}"),
            },
        },
        rocks_perf_sample_interval: value_t_or_exit!(
            matches,
            "rocksdb_perf_sample_interval",
            usize
        ),
    };

    let blockstore_options = BlockstoreOptions {
        recovery_mode,
        column_options,
        // The validator needs to open many files, check that the process has
        // permission to do so in order to fail quickly and give a direct error
        enforce_ulimit_nofile: true,
        // The validator needs primary (read/write)
        access_type: AccessType::Primary,
        num_rocksdb_compaction_threads: rocksdb_compaction_threads,
        num_rocksdb_flush_threads: rocksdb_flush_threads,
    };

    let accounts_hash_cache_path = matches
        .value_of("accounts_hash_cache_path")
        .map(Into::into)
        .unwrap_or_else(|| ledger_path.join(AccountsDb::DEFAULT_ACCOUNTS_HASH_CACHE_DIR));
    let accounts_hash_cache_path = create_and_canonicalize_directory(&accounts_hash_cache_path)
        .map_err(|err| {
            format!(
                "Unable to access accounts hash cache path '{}': {err}",
                accounts_hash_cache_path.display(),
            )
        })?;

    let debug_keys: Option<Arc<HashSet<_>>> = if matches.is_present("debug_key") {
        Some(Arc::new(
            values_t_or_exit!(matches, "debug_key", Pubkey)
                .into_iter()
                .collect(),
        ))
    } else {
        None
    };

    let known_validators = validators_set(
        &identity_keypair.pubkey(),
        matches,
        "known_validators",
        "--known-validator",
    )?;
    let repair_validators = validators_set(
        &identity_keypair.pubkey(),
        matches,
        "repair_validators",
        "--repair-validator",
    )?;
    let repair_whitelist = validators_set(
        &identity_keypair.pubkey(),
        matches,
        "repair_whitelist",
        "--repair-whitelist",
    )?;
    let repair_whitelist = Arc::new(RwLock::new(repair_whitelist.unwrap_or_default()));
    let gossip_validators = validators_set(
        &identity_keypair.pubkey(),
        matches,
        "gossip_validators",
        "--gossip-validator",
    )?;

    let bind_address = solana_net_utils::parse_host(matches.value_of("bind_address").unwrap())
        .expect("invalid bind_address");
    let rpc_bind_address = if matches.is_present("rpc_bind_address") {
        solana_net_utils::parse_host(matches.value_of("rpc_bind_address").unwrap())
            .expect("invalid rpc_bind_address")
    } else if private_rpc {
        solana_net_utils::parse_host("127.0.0.1").unwrap()
    } else {
        bind_address
    };

    let contact_debug_interval = value_t_or_exit!(matches, "contact_debug_interval", u64);

    let account_indexes = process_account_indexes(matches);

    let restricted_repair_only_mode = matches.is_present("restricted_repair_only_mode");
    let accounts_shrink_optimize_total_space =
        value_t_or_exit!(matches, "accounts_shrink_optimize_total_space", bool);
    let tpu_use_quic = !matches.is_present("tpu_disable_quic");
    let vote_use_quic = value_t_or_exit!(matches, "vote_use_quic", bool);

    let tpu_enable_udp = if matches.is_present("tpu_enable_udp") {
        true
    } else {
        DEFAULT_TPU_ENABLE_UDP
    };

    let tpu_connection_pool_size = value_t_or_exit!(matches, "tpu_connection_pool_size", usize);

    let shrink_ratio = value_t_or_exit!(matches, "accounts_shrink_ratio", f64);
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
    let entrypoint_addrs = values_t!(matches, "entrypoint", String)
        .unwrap_or_default()
        .into_iter()
        .map(|entrypoint| {
            solana_net_utils::parse_host_port(&entrypoint)
                .map_err(|err| format!("failed to parse entrypoint address: {err}"))
        })
        .collect::<Result<HashSet<_>, _>>()?
        .into_iter()
        .collect::<Vec<_>>();
    for addr in &entrypoint_addrs {
        if !socket_addr_space.check(addr) {
            Err(format!("invalid entrypoint address: {addr}"))?;
        }
    }
    // TODO: Once entrypoints are updated to return shred-version, this should
    // abort if it fails to obtain a shred-version, so that nodes always join
    // gossip with a valid shred-version. The code to adopt entrypoint shred
    // version can then be deleted from gossip and get_rpc_node above.
    let expected_shred_version = value_t!(matches, "expected_shred_version", u16)
        .ok()
        .or_else(|| get_cluster_shred_version(&entrypoint_addrs, bind_address));

    let tower_storage: Arc<dyn tower_storage::TowerStorage> =
        match value_t_or_exit!(matches, "tower_storage", String).as_str() {
            "file" => {
                let tower_path = value_t!(matches, "tower", PathBuf)
                    .ok()
                    .unwrap_or_else(|| ledger_path.clone());

                Arc::new(tower_storage::FileTowerStorage::new(tower_path))
            }
            "etcd" => {
                let endpoints = values_t_or_exit!(matches, "etcd_endpoint", String);
                let domain_name = value_t_or_exit!(matches, "etcd_domain_name", String);
                let ca_certificate_file = value_t_or_exit!(matches, "etcd_cacert_file", String);
                let identity_certificate_file = value_t_or_exit!(matches, "etcd_cert_file", String);
                let identity_private_key_file = value_t_or_exit!(matches, "etcd_key_file", String);

                let read =
                    |file| fs::read(&file).map_err(|err| format!("unable to read {file}: {err}"));

                let tls_config = tower_storage::EtcdTlsConfig {
                    domain_name,
                    ca_certificate: read(ca_certificate_file)?,
                    identity_certificate: read(identity_certificate_file)?,
                    identity_private_key: read(identity_private_key_file)?,
                };

                Arc::new(
                    tower_storage::EtcdTowerStorage::new(endpoints, Some(tls_config))
                        .map_err(|err| format!("failed to connect to etcd: {err}"))?,
                )
            }
            _ => unreachable!(),
        };

    let mut accounts_index_config = AccountsIndexConfig {
        num_flush_threads: Some(accounts_index_flush_threads),
        ..AccountsIndexConfig::default()
    };
    if let Ok(bins) = value_t!(matches, "accounts_index_bins", usize) {
        accounts_index_config.bins = Some(bins);
    }

    accounts_index_config.index_limit_mb = if matches.is_present("disable_accounts_disk_index") {
        IndexLimitMb::InMemOnly
    } else {
        IndexLimitMb::Unlimited
    };

    {
        let mut accounts_index_paths: Vec<PathBuf> = if matches.is_present("accounts_index_path") {
            values_t_or_exit!(matches, "accounts_index_path", String)
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
    accounts_index_config.scan_results_limit_bytes =
        value_t!(matches, "accounts_index_scan_results_limit_mb", usize)
            .ok()
            .map(|mb| mb * MB);

    let account_shrink_paths: Option<Vec<PathBuf>> =
        values_t!(matches, "account_shrink_path", String)
            .map(|shrink_paths| shrink_paths.into_iter().map(PathBuf::from).collect())
            .ok();
    let account_shrink_paths = account_shrink_paths
        .as_ref()
        .map(|paths| {
            create_and_canonicalize_directories(paths)
                .map_err(|err| format!("unable to access account shrink path: {err}"))
        })
        .transpose()?;

    let (account_shrink_run_paths, account_shrink_snapshot_paths) = account_shrink_paths
        .map(|paths| {
            create_all_accounts_run_and_snapshot_dirs(&paths)
                .map_err(|err| format!("unable to create account subdirectories: {err}"))
        })
        .transpose()?
        .unzip();

    let read_cache_limit_bytes = values_of::<usize>(matches, "accounts_db_read_cache_limit_mb")
        .map(|limits| {
            match limits.len() {
                // we were given explicit low and high watermark values, so use them
                2 => (limits[0] * MB, limits[1] * MB),
                // we were given a single value, so use it for both low and high watermarks
                1 => (limits[0] * MB, limits[0] * MB),
                _ => {
                    // clap will enforce either one or two values is given
                    unreachable!(
                        "invalid number of values given to accounts-db-read-cache-limit-mb"
                    )
                }
            }
        });
    let create_ancient_storage = matches
        .value_of("accounts_db_squash_storages_method")
        .map(|method| match method {
            "pack" => CreateAncientStorage::Pack,
            "append" => CreateAncientStorage::Append,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts-db-squash-storages-method")
            }
        })
        .unwrap_or_default();
    let storage_access = matches
        .value_of("accounts_db_access_storages_method")
        .map(|method| match method {
            "mmap" => StorageAccess::Mmap,
            "file" => StorageAccess::File,
            _ => {
                // clap will enforce one of the above values is given
                unreachable!("invalid value given to accounts-db-access-storages-method")
            }
        })
        .unwrap_or_default();

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

    let accounts_db_config = AccountsDbConfig {
        index: Some(accounts_index_config),
        account_indexes: Some(account_indexes.clone()),
        base_working_path: Some(ledger_path.clone()),
        accounts_hash_cache_path: Some(accounts_hash_cache_path),
        shrink_paths: account_shrink_run_paths,
        shrink_ratio,
        read_cache_limit_bytes,
        write_cache_limit_bytes: value_t!(matches, "accounts_db_cache_limit_mb", u64)
            .ok()
            .map(|mb| mb * MB as u64),
        ancient_append_vec_offset: value_t!(matches, "accounts_db_ancient_append_vecs", i64).ok(),
        ancient_storage_ideal_size: value_t!(
            matches,
            "accounts_db_ancient_storage_ideal_size",
            u64
        )
        .ok(),
        max_ancient_storages: value_t!(matches, "accounts_db_max_ancient_storages", usize).ok(),
        hash_calculation_pubkey_bins: value_t!(
            matches,
            "accounts_db_hash_calculation_pubkey_bins",
            usize
        )
        .ok(),
        exhaustively_verify_refcounts: matches.is_present("accounts_db_verify_refcounts"),
        create_ancient_storage,
        test_skip_rewrites_but_include_in_bank_hash: matches
            .is_present("accounts_db_test_skip_rewrites"),
        storage_access,
        scan_filter_for_shrinking,
        enable_experimental_accumulator_hash: matches
            .is_present("accounts_db_experimental_accumulator_hash"),
        verify_experimental_accumulator_hash: matches
            .is_present("accounts_db_verify_experimental_accumulator_hash"),
        snapshots_use_experimental_accumulator_hash: matches
            .is_present("accounts_db_snapshots_use_experimental_accumulator_hash"),
        num_clean_threads: Some(accounts_db_clean_threads),
        num_foreground_threads: Some(accounts_db_foreground_threads),
        num_hash_threads: Some(accounts_db_hash_threads),
        ..AccountsDbConfig::default()
    };

    let accounts_db_config = Some(accounts_db_config);

    let on_start_geyser_plugin_config_files = if matches.is_present("geyser_plugin_config") {
        Some(
            values_t_or_exit!(matches, "geyser_plugin_config", String)
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        )
    } else {
        None
    };
    let starting_with_geyser_plugins: bool = on_start_geyser_plugin_config_files.is_some()
        || matches.is_present("geyser_plugin_always_enabled");

    let rpc_bigtable_config = if matches.is_present("enable_rpc_bigtable_ledger_storage")
        || matches.is_present("enable_bigtable_ledger_upload")
    {
        Some(RpcBigtableConfig {
            enable_bigtable_ledger_upload: matches.is_present("enable_bigtable_ledger_upload"),
            bigtable_instance_name: value_t_or_exit!(matches, "rpc_bigtable_instance_name", String),
            bigtable_app_profile_id: value_t_or_exit!(
                matches,
                "rpc_bigtable_app_profile_id",
                String
            ),
            timeout: value_t!(matches, "rpc_bigtable_timeout", u64)
                .ok()
                .map(Duration::from_secs),
            max_message_size: value_t_or_exit!(matches, "rpc_bigtable_max_message_size", usize),
        })
    } else {
        None
    };

    let rpc_send_retry_rate_ms = value_t_or_exit!(matches, "rpc_send_transaction_retry_ms", u64);
    let rpc_send_batch_size = value_t_or_exit!(matches, "rpc_send_transaction_batch_size", usize);
    let rpc_send_batch_send_rate_ms =
        value_t_or_exit!(matches, "rpc_send_transaction_batch_ms", u64);

    if rpc_send_batch_send_rate_ms > rpc_send_retry_rate_ms {
        Err(format!(
            "the specified rpc-send-batch-ms ({rpc_send_batch_send_rate_ms}) is invalid, it must \
             be <= rpc-send-retry-ms ({rpc_send_retry_rate_ms})"
        ))?;
    }

    let tps = rpc_send_batch_size as u64 * MILLIS_PER_SECOND / rpc_send_batch_send_rate_ms;
    if tps > send_transaction_service::MAX_TRANSACTION_SENDS_PER_SECOND {
        Err(format!(
            "either the specified rpc-send-batch-size ({}) or rpc-send-batch-ms ({}) is invalid, \
             'rpc-send-batch-size * 1000 / rpc-send-batch-ms' must be smaller than ({}) .",
            rpc_send_batch_size,
            rpc_send_batch_send_rate_ms,
            send_transaction_service::MAX_TRANSACTION_SENDS_PER_SECOND
        ))?;
    }
    let rpc_send_transaction_tpu_peers = matches
        .values_of("rpc_send_transaction_tpu_peer")
        .map(|values| {
            values
                .map(solana_net_utils::parse_host_port)
                .collect::<Result<Vec<SocketAddr>, String>>()
        })
        .transpose()
        .map_err(|err| {
            format!("failed to parse rpc send-transaction-service tpu peer address: {err}")
        })?;
    let rpc_send_transaction_also_leader = matches.is_present("rpc_send_transaction_also_leader");
    let leader_forward_count =
        if rpc_send_transaction_tpu_peers.is_some() && !rpc_send_transaction_also_leader {
            // rpc-sts is configured to send only to specific tpu peers. disable leader forwards
            0
        } else {
            value_t_or_exit!(matches, "rpc_send_transaction_leader_forward_count", u64)
        };

    let full_api = matches.is_present("full_rpc_api");

    let mut validator_config = ValidatorConfig {
        require_tower: matches.is_present("require_tower"),
        tower_storage,
        halt_at_slot: value_t!(matches, "dev_halt_at_slot", Slot).ok(),
        expected_genesis_hash: matches
            .value_of("expected_genesis_hash")
            .map(|s| Hash::from_str(s).unwrap()),
        expected_bank_hash: matches
            .value_of("expected_bank_hash")
            .map(|s| Hash::from_str(s).unwrap()),
        expected_shred_version,
        new_hard_forks: hardforks_of(matches, "hard_forks"),
        rpc_config: JsonRpcConfig {
            enable_rpc_transaction_history: matches.is_present("enable_rpc_transaction_history"),
            enable_extended_tx_metadata_storage: matches.is_present("enable_cpi_and_log_storage")
                || matches.is_present("enable_extended_tx_metadata_storage"),
            rpc_bigtable_config,
            faucet_addr: matches.value_of("rpc_faucet_addr").map(|address| {
                solana_net_utils::parse_host_port(address).expect("failed to parse faucet address")
            }),
            full_api,
            max_multiple_accounts: Some(value_t_or_exit!(
                matches,
                "rpc_max_multiple_accounts",
                usize
            )),
            health_check_slot_distance: value_t_or_exit!(
                matches,
                "health_check_slot_distance",
                u64
            ),
            disable_health_check: false,
            rpc_threads: value_t_or_exit!(matches, "rpc_threads", usize),
            rpc_blocking_threads: value_t_or_exit!(matches, "rpc_blocking_threads", usize),
            rpc_niceness_adj: value_t_or_exit!(matches, "rpc_niceness_adj", i8),
            account_indexes: account_indexes.clone(),
            rpc_scan_and_fix_roots: matches.is_present("rpc_scan_and_fix_roots"),
            max_request_body_size: Some(value_t_or_exit!(
                matches,
                "rpc_max_request_body_size",
                usize
            )),
            skip_preflight_health_check: matches.is_present("skip_preflight_health_check"),
        },
        on_start_geyser_plugin_config_files,
        geyser_plugin_always_enabled: matches.is_present("geyser_plugin_always_enabled"),
        rpc_addrs: value_t!(matches, "rpc_port", u16).ok().map(|rpc_port| {
            (
                SocketAddr::new(rpc_bind_address, rpc_port),
                SocketAddr::new(rpc_bind_address, rpc_port + 1),
                // If additional ports are added, +2 needs to be skipped to avoid a conflict with
                // the websocket port (which is +2) in web3.js This odd port shifting is tracked at
                // https://github.com/solana-labs/solana/issues/12250
            )
        }),
        pubsub_config: PubSubConfig {
            enable_block_subscription: matches.is_present("rpc_pubsub_enable_block_subscription"),
            enable_vote_subscription: matches.is_present("rpc_pubsub_enable_vote_subscription"),
            max_active_subscriptions: value_t_or_exit!(
                matches,
                "rpc_pubsub_max_active_subscriptions",
                usize
            ),
            queue_capacity_items: value_t_or_exit!(
                matches,
                "rpc_pubsub_queue_capacity_items",
                usize
            ),
            queue_capacity_bytes: value_t_or_exit!(
                matches,
                "rpc_pubsub_queue_capacity_bytes",
                usize
            ),
            worker_threads: value_t_or_exit!(matches, "rpc_pubsub_worker_threads", usize),
            notification_threads: value_t!(matches, "rpc_pubsub_notification_threads", usize)
                .ok()
                .and_then(NonZeroUsize::new),
        },
        voting_disabled: matches.is_present("no_voting") || restricted_repair_only_mode,
        wait_for_supermajority: value_t!(matches, "wait_for_supermajority", Slot).ok(),
        known_validators,
        repair_validators,
        repair_whitelist,
        gossip_validators,
        max_ledger_shreds,
        blockstore_options,
        run_verification: !(matches.is_present("skip_poh_verify")
            || matches.is_present("skip_startup_ledger_verification")),
        debug_keys,
        contact_debug_interval,
        send_transaction_service_config: send_transaction_service::Config {
            retry_rate_ms: rpc_send_retry_rate_ms,
            leader_forward_count,
            default_max_retries: value_t!(
                matches,
                "rpc_send_transaction_default_max_retries",
                usize
            )
            .ok(),
            service_max_retries: value_t_or_exit!(
                matches,
                "rpc_send_transaction_service_max_retries",
                usize
            ),
            batch_send_rate_ms: rpc_send_batch_send_rate_ms,
            batch_size: rpc_send_batch_size,
            retry_pool_max_size: value_t_or_exit!(
                matches,
                "rpc_send_transaction_retry_pool_max_size",
                usize
            ),
            tpu_peers: rpc_send_transaction_tpu_peers,
        },
        no_poh_speed_test: matches.is_present("no_poh_speed_test"),
        no_os_memory_stats_reporting: matches.is_present("no_os_memory_stats_reporting"),
        no_os_network_stats_reporting: matches.is_present("no_os_network_stats_reporting"),
        no_os_cpu_stats_reporting: matches.is_present("no_os_cpu_stats_reporting"),
        no_os_disk_stats_reporting: matches.is_present("no_os_disk_stats_reporting"),
        poh_pinned_cpu_core: value_of(matches, "poh_pinned_cpu_core")
            .unwrap_or(poh_service::DEFAULT_PINNED_CPU_CORE),
        poh_hashes_per_batch: value_of(matches, "poh_hashes_per_batch")
            .unwrap_or(poh_service::DEFAULT_HASHES_PER_BATCH),
        process_ledger_before_services: matches.is_present("process_ledger_before_services"),
        accounts_db_test_hash_calculation: matches.is_present("accounts_db_test_hash_calculation"),
        accounts_db_config,
        accounts_db_skip_shrink: true,
        accounts_db_force_initial_clean: matches.is_present("no_skip_initial_accounts_db_clean"),
        tpu_coalesce,
        no_wait_for_vote_to_start_leader: matches.is_present("no_wait_for_vote_to_start_leader"),
        runtime_config: RuntimeConfig {
            log_messages_bytes_limit: value_of(matches, "log_messages_bytes_limit"),
            ..RuntimeConfig::default()
        },
        staked_nodes_overrides: staked_nodes_overrides.clone(),
        use_snapshot_archives_at_startup: value_t_or_exit!(
            matches,
            use_snapshot_archives_at_startup::cli::NAME,
            UseSnapshotArchivesAtStartup
        ),
        ip_echo_server_threads,
        rayon_global_threads,
        replay_forks_threads,
        replay_transactions_threads,
        tvu_shred_sigverify_threads: tvu_sigverify_threads,
        delay_leader_block_for_pending_fork: matches
            .is_present("delay_leader_block_for_pending_fork"),
        wen_restart_proto_path: value_t!(matches, "wen_restart", PathBuf).ok(),
        wen_restart_coordinator: value_t!(matches, "wen_restart_coordinator", Pubkey).ok(),
        ..ValidatorConfig::default()
    };

    let vote_account = pubkey_of(matches, "vote_account").unwrap_or_else(|| {
        if !validator_config.voting_disabled {
            warn!("--vote-account not specified, validator will not vote");
            validator_config.voting_disabled = true;
        }
        Keypair::new().pubkey()
    });

    let dynamic_port_range =
        solana_net_utils::parse_port_range(matches.value_of("dynamic_port_range").unwrap())
            .expect("invalid dynamic_port_range");

    let account_paths: Vec<PathBuf> =
        if let Ok(account_paths) = values_t!(matches, "account_paths", String) {
            account_paths
                .join(",")
                .split(',')
                .map(PathBuf::from)
                .collect()
        } else {
            vec![ledger_path.join("accounts")]
        };
    let account_paths = create_and_canonicalize_directories(account_paths)
        .map_err(|err| format!("unable to access account path: {err}"))?;

    let (account_run_paths, account_snapshot_paths) =
        create_all_accounts_run_and_snapshot_dirs(&account_paths)
            .map_err(|err| format!("unable to create account directories: {err}"))?;

    // From now on, use run/ paths in the same way as the previous account_paths.
    validator_config.account_paths = account_run_paths;

    // These snapshot paths are only used for initial clean up, add in shrink paths if they exist.
    validator_config.account_snapshot_paths =
        if let Some(account_shrink_snapshot_paths) = account_shrink_snapshot_paths {
            account_snapshot_paths
                .into_iter()
                .chain(account_shrink_snapshot_paths)
                .collect()
        } else {
            account_snapshot_paths
        };

    let maximum_local_snapshot_age = value_t_or_exit!(matches, "maximum_local_snapshot_age", u64);
    let maximum_full_snapshot_archives_to_retain =
        value_t_or_exit!(matches, "maximum_full_snapshots_to_retain", NonZeroUsize);
    let maximum_incremental_snapshot_archives_to_retain = value_t_or_exit!(
        matches,
        "maximum_incremental_snapshots_to_retain",
        NonZeroUsize
    );
    let snapshot_packager_niceness_adj =
        value_t_or_exit!(matches, "snapshot_packager_niceness_adj", i8);
    let minimal_snapshot_download_speed =
        value_t_or_exit!(matches, "minimal_snapshot_download_speed", f32);
    let maximum_snapshot_download_abort =
        value_t_or_exit!(matches, "maximum_snapshot_download_abort", u64);

    let snapshots_dir = if let Some(snapshots) = matches.value_of("snapshots") {
        Path::new(snapshots)
    } else {
        &ledger_path
    };
    let snapshots_dir = create_and_canonicalize_directory(snapshots_dir).map_err(|err| {
        format!(
            "failed to create snapshots directory '{}': {err}",
            snapshots_dir.display(),
        )
    })?;

    if account_paths
        .iter()
        .any(|account_path| account_path == &snapshots_dir)
    {
        Err(
            "the --accounts and --snapshots paths must be unique since they \
             both create 'snapshots' subdirectories, otherwise there may be collisions"
                .to_string(),
        )?;
    }

    let bank_snapshots_dir = snapshots_dir.join("snapshots");
    fs::create_dir_all(&bank_snapshots_dir).map_err(|err| {
        format!(
            "failed to create bank snapshots directory '{}': {err}",
            bank_snapshots_dir.display(),
        )
    })?;

    let full_snapshot_archives_dir =
        if let Some(full_snapshot_archive_path) = matches.value_of("full_snapshot_archive_path") {
            PathBuf::from(full_snapshot_archive_path)
        } else {
            snapshots_dir.clone()
        };
    fs::create_dir_all(&full_snapshot_archives_dir).map_err(|err| {
        format!(
            "failed to create full snapshot archives directory '{}': {err}",
            full_snapshot_archives_dir.display(),
        )
    })?;

    let incremental_snapshot_archives_dir = if let Some(incremental_snapshot_archive_path) =
        matches.value_of("incremental_snapshot_archive_path")
    {
        PathBuf::from(incremental_snapshot_archive_path)
    } else {
        snapshots_dir.clone()
    };
    fs::create_dir_all(&incremental_snapshot_archives_dir).map_err(|err| {
        format!(
            "failed to create incremental snapshot archives directory '{}': {err}",
            incremental_snapshot_archives_dir.display(),
        )
    })?;

    let archive_format = {
        let archive_format_str = value_t_or_exit!(matches, "snapshot_archive_format", String);
        let mut archive_format = ArchiveFormat::from_cli_arg(&archive_format_str)
            .unwrap_or_else(|| panic!("Archive format not recognized: {archive_format_str}"));
        if let ArchiveFormat::TarZstd { config } = &mut archive_format {
            config.compression_level =
                value_t_or_exit!(matches, "snapshot_zstd_compression_level", i32);
        }
        archive_format
    };

    let snapshot_version = matches
        .value_of("snapshot_version")
        .map(|value| {
            value
                .parse::<SnapshotVersion>()
                .map_err(|err| format!("unable to parse snapshot version: {err}"))
        })
        .transpose()?
        .unwrap_or(SnapshotVersion::default());

    let (full_snapshot_archive_interval_slots, incremental_snapshot_archive_interval_slots) =
        if matches.is_present("no_snapshots") {
            // snapshots are disabled
            (
                DISABLED_SNAPSHOT_ARCHIVE_INTERVAL,
                DISABLED_SNAPSHOT_ARCHIVE_INTERVAL,
            )
        } else {
            match (
                !matches.is_present("no_incremental_snapshots"),
                value_t_or_exit!(matches, "snapshot_interval_slots", u64),
            ) {
                (_, 0) => {
                    // snapshots are disabled
                    warn!(
                        "Snapshot generation was disabled with `--snapshot-interval-slots 0`, \
                         which is now deprecated. Use `--no-snapshots` instead.",
                    );
                    (
                        DISABLED_SNAPSHOT_ARCHIVE_INTERVAL,
                        DISABLED_SNAPSHOT_ARCHIVE_INTERVAL,
                    )
                }
                (true, incremental_snapshot_interval_slots) => {
                    // incremental snapshots are enabled
                    // use --snapshot-interval-slots for the incremental snapshot interval
                    (
                        value_t_or_exit!(matches, "full_snapshot_interval_slots", u64),
                        incremental_snapshot_interval_slots,
                    )
                }
                (false, full_snapshot_interval_slots) => {
                    // incremental snapshots are *disabled*
                    // use --snapshot-interval-slots for the *full* snapshot interval
                    // also warn if --full-snapshot-interval-slots was specified
                    if matches.occurrences_of("full_snapshot_interval_slots") > 0 {
                        warn!(
                            "Incremental snapshots are disabled, yet --full-snapshot-interval-slots was specified! \
                             Note that --full-snapshot-interval-slots is *ignored* when incremental snapshots are disabled. \
                             Use --snapshot-interval-slots instead.",
                        );
                    }
                    (
                        full_snapshot_interval_slots,
                        DISABLED_SNAPSHOT_ARCHIVE_INTERVAL,
                    )
                }
            }
        };

    validator_config.snapshot_config = SnapshotConfig {
        usage: if full_snapshot_archive_interval_slots == DISABLED_SNAPSHOT_ARCHIVE_INTERVAL {
            SnapshotUsage::LoadOnly
        } else {
            SnapshotUsage::LoadAndGenerate
        },
        full_snapshot_archive_interval_slots,
        incremental_snapshot_archive_interval_slots,
        bank_snapshots_dir,
        full_snapshot_archives_dir: full_snapshot_archives_dir.clone(),
        incremental_snapshot_archives_dir: incremental_snapshot_archives_dir.clone(),
        archive_format,
        snapshot_version,
        maximum_full_snapshot_archives_to_retain,
        maximum_incremental_snapshot_archives_to_retain,
        packager_thread_niceness_adj: snapshot_packager_niceness_adj,
    };

    info!(
        "Snapshot configuration: full snapshot interval: {}, incremental snapshot interval: {}",
        if full_snapshot_archive_interval_slots == DISABLED_SNAPSHOT_ARCHIVE_INTERVAL {
            "disabled".to_string()
        } else {
            format!("{full_snapshot_archive_interval_slots} slots")
        },
        if incremental_snapshot_archive_interval_slots == DISABLED_SNAPSHOT_ARCHIVE_INTERVAL {
            "disabled".to_string()
        } else {
            format!("{incremental_snapshot_archive_interval_slots} slots")
        },
    );

    // It is unlikely that a full snapshot interval greater than an epoch is a good idea.
    // Minimally we should warn the user in case this was a mistake.
    if full_snapshot_archive_interval_slots > DEFAULT_SLOTS_PER_EPOCH
        && full_snapshot_archive_interval_slots != DISABLED_SNAPSHOT_ARCHIVE_INTERVAL
    {
        warn!(
            "The full snapshot interval is excessively large: {}! This will negatively \
            impact the background cleanup tasks in accounts-db. Consider a smaller value.",
            full_snapshot_archive_interval_slots,
        );
    }

    if !is_snapshot_config_valid(&validator_config.snapshot_config) {
        Err(
            "invalid snapshot configuration provided: snapshot intervals are incompatible. \
             \n\t- full snapshot interval MUST be larger than incremental snapshot interval \
             (if enabled)"
                .to_string(),
        )?;
    }

    configure_banking_trace_dir_byte_limit(&mut validator_config, matches);
    validator_config.block_verification_method = value_t!(
        matches,
        "block_verification_method",
        BlockVerificationMethod
    )
    .unwrap_or_default();
    validator_config.block_production_method = value_t!(
        matches, // comment to align formatting...
        "block_production_method",
        BlockProductionMethod
    )
    .unwrap_or_default();
    validator_config.transaction_struct = value_t!(
        matches, // comment to align formatting...
        "transaction_struct",
        TransactionStructure
    )
    .unwrap_or_default();
    validator_config.enable_block_production_forwarding = staked_nodes_overrides_path.is_some();
    validator_config.unified_scheduler_handler_threads =
        value_t!(matches, "unified_scheduler_handler_threads", usize).ok();

    let public_rpc_addr = matches
        .value_of("public_rpc_addr")
        .map(|addr| {
            solana_net_utils::parse_host_port(addr)
                .map_err(|err| format!("failed to parse public rpc address: {err}"))
        })
        .transpose()?;

    if !matches.is_present("no_os_network_limits_test") {
        if SystemMonitorService::check_os_network_limits() {
            info!("OS network limits test passed.");
        } else {
            Err("OS network limit test failed. See \
                https://docs.solanalabs.com/operations/guides/validator-start#system-tuning"
                .to_string())?;
        }
    }

    let mut ledger_lock = ledger_lockfile(&ledger_path);
    let _ledger_write_guard = lock_ledger(&ledger_path, &mut ledger_lock);

    let start_progress = Arc::new(RwLock::new(ValidatorStartProgress::default()));
    let admin_service_post_init = Arc::new(RwLock::new(None));
    let (rpc_to_plugin_manager_sender, rpc_to_plugin_manager_receiver) =
        if starting_with_geyser_plugins {
            let (sender, receiver) = unbounded();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
    admin_rpc_service::run(
        &ledger_path,
        admin_rpc_service::AdminRpcRequestMetadata {
            rpc_addr: validator_config.rpc_addrs.map(|(rpc_addr, _)| rpc_addr),
            start_time: std::time::SystemTime::now(),
            validator_exit: validator_config.validator_exit.clone(),
            start_progress: start_progress.clone(),
            authorized_voter_keypairs: authorized_voter_keypairs.clone(),
            post_init: admin_service_post_init.clone(),
            tower_storage: validator_config.tower_storage.clone(),
            staked_nodes_overrides,
            rpc_to_plugin_manager_sender,
        },
    );

    let gossip_host: IpAddr = matches
        .value_of("gossip_host")
        .map(|gossip_host| {
            solana_net_utils::parse_host(gossip_host)
                .map_err(|err| format!("failed to parse --gossip-host: {err}"))
        })
        .transpose()?
        .or_else(|| {
            if !entrypoint_addrs.is_empty() {
                let mut order: Vec<_> = (0..entrypoint_addrs.len()).collect();
                order.shuffle(&mut thread_rng());
                // Return once we determine our IP from an entrypoint
                order.into_iter().find_map(|i| {
                    let entrypoint_addr = &entrypoint_addrs[i];
                    info!(
                        "Contacting {} to determine the validator's public IP address",
                        entrypoint_addr
                    );
                    solana_net_utils::get_public_ip_addr_with_binding(entrypoint_addr, bind_address)
                        .map_or_else(
                            |err| {
                                warn!(
                                    "Failed to contact cluster entrypoint {entrypoint_addr}: {err}"
                                );
                                None
                            },
                            Some,
                        )
                })
            } else {
                Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
            }
        })
        .ok_or_else(|| "unable to determine the validator's public IP address".to_string())?;
    let gossip_port = value_t!(matches, "gossip_port", u16).or_else(|_| {
        solana_net_utils::find_available_port_in_range(bind_address, (0, 1))
            .map_err(|err| format!("unable to find an available gossip port: {err}"))
    })?;
    let gossip_addr = SocketAddr::new(gossip_host, gossip_port);

    let public_tpu_addr = matches
        .value_of("public_tpu_addr")
        .map(|public_tpu_addr| {
            solana_net_utils::parse_host_port(public_tpu_addr)
                .map_err(|err| format!("failed to parse --public-tpu-address: {err}"))
        })
        .transpose()?;

    let public_tpu_forwards_addr = matches
        .value_of("public_tpu_forwards_addr")
        .map(|public_tpu_forwards_addr| {
            solana_net_utils::parse_host_port(public_tpu_forwards_addr)
                .map_err(|err| format!("failed to parse --public-tpu-forwards-address: {err}"))
        })
        .transpose()?;

    let num_quic_endpoints = value_t_or_exit!(matches, "num_quic_endpoints", NonZeroUsize);

    let tpu_max_connections_per_peer =
        value_t_or_exit!(matches, "tpu_max_connections_per_peer", u64);
    let tpu_max_staked_connections = value_t_or_exit!(matches, "tpu_max_staked_connections", u64);
    let tpu_max_unstaked_connections =
        value_t_or_exit!(matches, "tpu_max_unstaked_connections", u64);

    let tpu_max_fwd_staked_connections =
        value_t_or_exit!(matches, "tpu_max_fwd_staked_connections", u64);
    let tpu_max_fwd_unstaked_connections =
        value_t_or_exit!(matches, "tpu_max_fwd_unstaked_connections", u64);

    let tpu_max_connections_per_ipaddr_per_minute: u64 =
        value_t_or_exit!(matches, "tpu_max_connections_per_ipaddr_per_minute", u64);
    let max_streams_per_ms = value_t_or_exit!(matches, "tpu_max_streams_per_ms", u64);

    let node_config = NodeConfig {
        gossip_addr,
        port_range: dynamic_port_range,
        bind_ip_addr: bind_address,
        public_tpu_addr,
        public_tpu_forwards_addr,
        num_tvu_receive_sockets: tvu_receive_threads,
        num_tvu_retransmit_sockets: tvu_retransmit_threads,
        num_quic_endpoints,
    };

    let cluster_entrypoints = entrypoint_addrs
        .iter()
        .map(ContactInfo::new_gossip_entry_point)
        .collect::<Vec<_>>();

    let mut node = Node::new_with_external_ip(&identity_keypair.pubkey(), node_config);

    if restricted_repair_only_mode {
        if validator_config.wen_restart_proto_path.is_some() {
            Err("--restricted-repair-only-mode is not compatible with --wen_restart".to_string())?;
        }

        // When in --restricted_repair_only_mode is enabled only the gossip and repair ports
        // need to be reachable by the entrypoint to respond to gossip pull requests and repair
        // requests initiated by the node.  All other ports are unused.
        node.info.remove_tpu();
        node.info.remove_tpu_forwards();
        node.info.remove_tvu();
        node.info.remove_serve_repair();

        // A node in this configuration shouldn't be an entrypoint to other nodes
        node.sockets.ip_echo = None;
    }

    if !private_rpc {
        macro_rules! set_socket {
            ($method:ident, $addr:expr, $name:literal) => {
                node.info.$method($addr).expect(&format!(
                    "Operator must spin up node with valid {} address",
                    $name
                ))
            };
        }
        if let Some(public_rpc_addr) = public_rpc_addr {
            set_socket!(set_rpc, public_rpc_addr, "RPC");
            set_socket!(set_rpc_pubsub, public_rpc_addr, "RPC-pubsub");
        } else if let Some((rpc_addr, rpc_pubsub_addr)) = validator_config.rpc_addrs {
            let addr = node
                .info
                .gossip()
                .expect("Operator must spin up node with valid gossip address")
                .ip();
            set_socket!(set_rpc, (addr, rpc_addr.port()), "RPC");
            set_socket!(set_rpc_pubsub, (addr, rpc_pubsub_addr.port()), "RPC-pubsub");
        }
    }

    solana_metrics::set_host_id(identity_keypair.pubkey().to_string());
    solana_metrics::set_panic_hook("validator", Some(String::from(solana_version)));
    solana_entry::entry::init_poh();
    snapshot_utils::remove_tmp_snapshot_archives(&full_snapshot_archives_dir);
    snapshot_utils::remove_tmp_snapshot_archives(&incremental_snapshot_archives_dir);

    let identity_keypair = Arc::new(identity_keypair);

    let should_check_duplicate_instance = true;
    if !cluster_entrypoints.is_empty() {
        bootstrap::rpc_bootstrap(
            &node,
            &identity_keypair,
            &ledger_path,
            &full_snapshot_archives_dir,
            &incremental_snapshot_archives_dir,
            &vote_account,
            authorized_voter_keypairs.clone(),
            &cluster_entrypoints,
            &mut validator_config,
            rpc_bootstrap_config,
            do_port_check,
            use_progress_bar,
            maximum_local_snapshot_age,
            should_check_duplicate_instance,
            &start_progress,
            minimal_snapshot_download_speed,
            maximum_snapshot_download_abort,
            socket_addr_space,
        );
        *start_progress.write().unwrap() = ValidatorStartProgress::Initializing;
    }

    if operation == Operation::Initialize {
        info!("Validator ledger initialization complete");
        return Ok(());
    }

    // Bootstrap code above pushes a contact-info with more recent timestamp to
    // gossip. If the node is staked the contact-info lingers in gossip causing
    // false duplicate nodes error.
    // Below line refreshes the timestamp on contact-info so that it overrides
    // the one pushed by bootstrap.
    node.info.hot_swap_pubkey(identity_keypair.pubkey());

    let tpu_quic_server_config = QuicServerParams {
        max_connections_per_peer: tpu_max_connections_per_peer.try_into().unwrap(),
        max_staked_connections: tpu_max_staked_connections.try_into().unwrap(),
        max_unstaked_connections: tpu_max_unstaked_connections.try_into().unwrap(),
        max_streams_per_ms,
        max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
        coalesce: tpu_coalesce,
        ..Default::default()
    };

    let tpu_fwd_quic_server_config = QuicServerParams {
        max_connections_per_peer: tpu_max_connections_per_peer.try_into().unwrap(),
        max_staked_connections: tpu_max_fwd_staked_connections.try_into().unwrap(),
        max_unstaked_connections: tpu_max_fwd_unstaked_connections.try_into().unwrap(),
        max_streams_per_ms,
        max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
        coalesce: tpu_coalesce,
        ..Default::default()
    };

    // Vote shares TPU forward's characteristics, except that we accept 1 connection
    // per peer and no unstaked connections are accepted.
    let mut vote_quic_server_config = tpu_fwd_quic_server_config.clone();
    vote_quic_server_config.max_connections_per_peer = 1;
    vote_quic_server_config.max_unstaked_connections = 0;

    let validator = match Validator::new(
        node,
        identity_keypair,
        &ledger_path,
        &vote_account,
        authorized_voter_keypairs,
        cluster_entrypoints,
        &validator_config,
        should_check_duplicate_instance,
        rpc_to_plugin_manager_receiver,
        start_progress,
        socket_addr_space,
        ValidatorTpuConfig {
            use_quic: tpu_use_quic,
            vote_use_quic,
            tpu_connection_pool_size,
            tpu_enable_udp,
            tpu_quic_server_config,
            tpu_fwd_quic_server_config,
            vote_quic_server_config,
        },
        admin_service_post_init,
    ) {
        Ok(validator) => Ok(validator),
        Err(err) => {
            if matches!(
                err.downcast_ref(),
                Some(&ValidatorError::WenRestartFinished)
            ) {
                // 200 is a special error code, see
                // https://github.com/solana-foundation/solana-improvement-documents/pull/46
                error!("Please remove --wen_restart and use --wait_for_supermajority as instructed above");
                exit(200);
            }
            Err(format!("{err:?}"))
        }
    }?;

    if let Some(filename) = init_complete_file {
        File::create(filename).map_err(|err| format!("unable to create {filename}: {err}"))?;
    }
    info!("Validator initialized");
    validator.join();
    info!("Validator exiting..");

    Ok(())
}

// This function is duplicated in ledger-tool/src/main.rs...
fn hardforks_of(matches: &ArgMatches<'_>, name: &str) -> Option<Vec<Slot>> {
    if matches.is_present(name) {
        Some(values_t_or_exit!(matches, name, Slot))
    } else {
        None
    }
}

fn validators_set(
    identity_pubkey: &Pubkey,
    matches: &ArgMatches<'_>,
    matches_name: &str,
    arg_name: &str,
) -> Result<Option<HashSet<Pubkey>>, String> {
    if matches.is_present(matches_name) {
        let validators_set: HashSet<_> = values_t_or_exit!(matches, matches_name, Pubkey)
            .into_iter()
            .collect();
        if validators_set.contains(identity_pubkey) {
            Err(format!(
                "the validator's identity pubkey cannot be a {arg_name}: {identity_pubkey}"
            ))?;
        }
        Ok(Some(validators_set))
    } else {
        Ok(None)
    }
}

fn get_cluster_shred_version(entrypoints: &[SocketAddr], bind_address: IpAddr) -> Option<u16> {
    let entrypoints = {
        let mut index: Vec<_> = (0..entrypoints.len()).collect();
        index.shuffle(&mut rand::thread_rng());
        index.into_iter().map(|i| &entrypoints[i])
    };
    for entrypoint in entrypoints {
        match solana_net_utils::get_cluster_shred_version_with_binding(entrypoint, bind_address) {
            Err(err) => eprintln!("get_cluster_shred_version failed: {entrypoint}, {err}"),
            Ok(0) => eprintln!("entrypoint {entrypoint} returned shred-version zero"),
            Ok(shred_version) => {
                info!(
                    "obtained shred-version {} from {}",
                    shred_version, entrypoint
                );
                return Some(shred_version);
            }
        }
    }
    None
}

fn configure_banking_trace_dir_byte_limit(
    validator_config: &mut ValidatorConfig,
    matches: &ArgMatches,
) {
    validator_config.banking_trace_dir_byte_limit = if matches.is_present("disable_banking_trace") {
        // disable with an explicit flag; This effectively becomes `opt-out` by resetting to
        // DISABLED_BAKING_TRACE_DIR, while allowing us to specify a default sensible limit in clap
        // configuration for cli help.
        DISABLED_BAKING_TRACE_DIR
    } else {
        // a default value in clap configuration (BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT) or
        // explicit user-supplied override value
        value_t_or_exit!(matches, "banking_trace_dir_byte_limit", u64)
    };
}

fn process_account_indexes(matches: &ArgMatches) -> AccountSecondaryIndexes {
    let account_indexes: HashSet<AccountIndex> = matches
        .values_of("account_indexes")
        .unwrap_or_default()
        .map(|value| match value {
            "program-id" => AccountIndex::ProgramId,
            "spl-token-mint" => AccountIndex::SplTokenMint,
            "spl-token-owner" => AccountIndex::SplTokenOwner,
            _ => unreachable!(),
        })
        .collect();

    let account_indexes_include_keys: HashSet<Pubkey> =
        values_t!(matches, "account_index_include_key", Pubkey)
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();

    let account_indexes_exclude_keys: HashSet<Pubkey> =
        values_t!(matches, "account_index_exclude_key", Pubkey)
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();

    let exclude_keys = !account_indexes_exclude_keys.is_empty();
    let include_keys = !account_indexes_include_keys.is_empty();

    let keys = if !account_indexes.is_empty() && (exclude_keys || include_keys) {
        let account_indexes_keys = AccountSecondaryIndexesIncludeExclude {
            exclude: exclude_keys,
            keys: if exclude_keys {
                account_indexes_exclude_keys
            } else {
                account_indexes_include_keys
            },
        };
        Some(account_indexes_keys)
    } else {
        None
    };

    AccountSecondaryIndexes {
        keys,
        indexes: account_indexes,
    }
}
