use {
    crate::{
        admin_rpc_service::{self, load_staked_nodes_overrides, StakedNodesOverrides},
        bootstrap,
        cli::{self},
        commands::{run::args::RunArgs, FromClapArgMatches},
        ledger_lockfile, lock_ledger,
    },
    agave_snapshots::{
        paths::BANK_SNAPSHOTS_DIR,
        snapshot_config::{SnapshotConfig, SnapshotUsage},
        ArchiveFormat, SnapshotInterval, SnapshotVersion,
    },
    clap::{crate_name, value_t, value_t_or_exit, values_t, values_t_or_exit, ArgMatches},
    crossbeam_channel::unbounded,
    log::*,
    rand::{seq::SliceRandom, thread_rng},
    solana_accounts_db::{
        accounts_db::{AccountShrinkThreshold, AccountsDbConfig, MarkObsoleteAccounts},
        accounts_file::StorageAccess,
        accounts_index::{AccountSecondaryIndexes, AccountsIndexConfig, IndexLimit, ScanFilter},
        utils::{
            create_all_accounts_run_and_snapshot_dirs, create_and_canonicalize_directories,
            create_and_canonicalize_directory,
        },
    },
    solana_clap_utils::input_parsers::{
        keypair_of, keypairs_of, parse_cpu_ranges, pubkey_of, value_of, values_of,
    },
    solana_clock::{Slot, DEFAULT_SLOTS_PER_EPOCH},
    solana_core::{
        banking_stage::transaction_scheduler::scheduler_controller::SchedulerConfig,
        banking_trace::DISABLED_BAKING_TRACE_DIR,
        consensus::tower_storage,
        repair::repair_handler::RepairHandlerType,
        snapshot_packager_service::SnapshotPackagerService,
        system_monitor_service::SystemMonitorService,
        tpu::MAX_VOTES_PER_SECOND,
        validator::{
            is_snapshot_config_valid, BlockProductionMethod, BlockVerificationMethod,
            SchedulerPacing, Validator, ValidatorConfig, ValidatorError, ValidatorStartProgress,
            ValidatorTpuConfig,
        },
    },
    solana_genesis_utils::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    solana_gossip::{
        cluster_info::{NodeConfig, DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS},
        contact_info::ContactInfo,
        node::Node,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_cleanup_service::{DEFAULT_MAX_LEDGER_SHREDS, DEFAULT_MIN_MAX_LEDGER_SHREDS},
        use_snapshot_archives_at_startup::{self, UseSnapshotArchivesAtStartup},
    },
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    solana_poh::poh_service,
    solana_pubkey::Pubkey,
    solana_runtime::{runtime_config::RuntimeConfig, snapshot_utils},
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::{simple_qos::SimpleQosConfig, swqos::SwQosConfig},
        quic::{QuicStreamerConfig, SimpleQosQuicStreamerConfig, SwQosQuicStreamerConfig},
    },
    solana_tpu_client::tpu_client::DEFAULT_TPU_ENABLE_UDP,
    solana_turbine::{
        broadcast_stage::BroadcastStageType,
        xdp::{set_cpu_affinity, XdpConfig},
    },
    solana_validator_exit::Exit,
    std::{
        collections::HashSet,
        fs::{self, File},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
        process::exit,
        str::{self, FromStr},
        sync::{atomic::AtomicBool, Arc, RwLock},
    },
};

#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    Initialize,
    Run,
}

pub fn execute(
    matches: &ArgMatches,
    solana_version: &str,
    operation: Operation,
) -> Result<(), Box<dyn std::error::Error>> {
    let run_args = RunArgs::from_clap_arg_match(matches)?;

    let cli::thread_args::NumThreadConfig {
        accounts_db_background_threads,
        accounts_db_foreground_threads,
        accounts_index_flush_threads,
        block_production_num_workers,
        ip_echo_server_threads,
        rayon_global_threads,
        replay_forks_threads,
        replay_transactions_threads,
        tpu_transaction_forward_receive_threads,
        tpu_transaction_receive_threads,
        tpu_vote_transaction_receive_threads,
        tvu_receive_threads,
        tvu_retransmit_threads,
        tvu_sigverify_threads,
    } = cli::thread_args::parse_num_threads_args(matches);

    let identity_keypair = Arc::new(run_args.identity_keypair);

    let logfile = run_args.logfile;
    if let Some(logfile) = logfile.as_ref() {
        println!("log file: {}", logfile.display());
    }
    let use_progress_bar = logfile.is_none();
    agave_logger::initialize_logging(logfile.clone());

    info!("{} {}", crate_name!(), solana_version);
    info!("Starting validator with: {:#?}", std::env::args_os());

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
                error!("Failed to load stake-nodes-overrides from {p}: {err}");
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

    let private_rpc = matches.is_present("private_rpc");
    let do_port_check = !matches.is_present("no_port_check");

    let ledger_path = run_args.ledger_path;

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

    let debug_keys: Option<Arc<HashSet<_>>> = if matches.is_present("debug_key") {
        Some(Arc::new(
            values_t_or_exit!(matches, "debug_key", Pubkey)
                .into_iter()
                .collect(),
        ))
    } else {
        None
    };

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

    let bind_addresses = {
        let parsed = matches
            .values_of("bind_address")
            .expect("bind_address should always be present due to default")
            .map(solana_net_utils::parse_host)
            .collect::<Result<Vec<_>, _>>()?;
        BindIpAddrs::new(parsed).map_err(|err| format!("invalid bind_addresses: {err}"))?
    };

    if bind_addresses.len() > 1 {
        for (flag, msg) in [
            (
                "advertised_ip",
                "--advertised-ip cannot be used in a multihoming context. In multihoming, the \
                 validator will advertise the first --bind-address as this node's public IP \
                 address.",
            ),
            (
                "tpu_vortexor_receiver_address",
                "--tpu-vortexor-receiver-address can not be used in a multihoming context",
            ),
            (
                "public_tpu_addr",
                "--public-tpu-address can not be used in a multihoming context",
            ),
        ] {
            if matches.is_present(flag) {
                Err(String::from(msg))?;
            }
        }
    }

    let rpc_bind_address = if matches.is_present("rpc_bind_address") {
        solana_net_utils::parse_host(matches.value_of("rpc_bind_address").unwrap())
            .expect("invalid rpc_bind_address")
    } else if private_rpc {
        solana_net_utils::parse_host("127.0.0.1").unwrap()
    } else {
        bind_addresses.active()
    };

    let contact_debug_interval = value_t_or_exit!(matches, "contact_debug_interval", u64);

    let account_indexes = AccountSecondaryIndexes::from_clap_arg_match(matches)?;

    let restricted_repair_only_mode = matches.is_present("restricted_repair_only_mode");
    let accounts_shrink_optimize_total_space =
        value_t_or_exit!(matches, "accounts_shrink_optimize_total_space", bool);
    let vote_use_quic = value_t_or_exit!(matches, "vote_use_quic", bool);

    let tpu_enable_udp = if matches.is_present("tpu_enable_udp") {
        warn!("Submission of TPU transactions via UDP is deprecated.");
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
    let entrypoint_addrs = run_args.entrypoints;
    for addr in &entrypoint_addrs {
        if !run_args.socket_addr_space.check(addr) {
            Err(format!("invalid entrypoint address: {addr}"))?;
        }
    }
    // TODO: Once entrypoints are updated to return shred-version, this should
    // abort if it fails to obtain a shred-version, so that nodes always join
    // gossip with a valid shred-version. The code to adopt entrypoint shred
    // version can then be deleted from gossip and get_rpc_node above.
    let expected_shred_version = value_t!(matches, "expected_shred_version", u16)
        .ok()
        .or_else(|| get_cluster_shred_version(&entrypoint_addrs, bind_addresses.active()));

    let tower_path = value_t!(matches, "tower", PathBuf)
        .ok()
        .unwrap_or_else(|| ledger_path.clone());
    let tower_storage: Arc<dyn tower_storage::TowerStorage> =
        Arc::new(tower_storage::FileTowerStorage::new(tower_path));

    let mut accounts_index_config = AccountsIndexConfig {
        num_flush_threads: Some(accounts_index_flush_threads),
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

    accounts_index_config.index_limit = if !matches.is_present("enable_accounts_disk_index") {
        IndexLimit::InMemOnly
    } else {
        IndexLimit::Minimal
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

    let read_cache_limit_bytes =
        values_of::<usize>(matches, "accounts_db_read_cache_limit").map(|limits| {
            match limits.len() {
                2 => (limits[0], limits[1]),
                _ => {
                    // clap will enforce two values are given
                    unreachable!("invalid number of values given to accounts-db-read-cache-limit")
                }
            }
        });

    let storage_access = matches
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

    let mark_obsolete_accounts = matches
        .value_of("accounts_db_mark_obsolete_accounts")
        .map(|mark_obsolete_accounts| {
            match mark_obsolete_accounts {
                "enabled" => MarkObsoleteAccounts::Enabled,
                "disabled" => MarkObsoleteAccounts::Disabled,
                _ => {
                    // clap will enforce one of the above values is given
                    unreachable!("invalid value given to accounts_db_mark_obsolete_accounts")
                }
            }
        })
        .unwrap_or_default();

    let accounts_db_config = AccountsDbConfig {
        index: Some(accounts_index_config),
        account_indexes: Some(account_indexes.clone()),
        base_working_path: Some(ledger_path.clone()),
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
        exhaustively_verify_refcounts: matches.is_present("accounts_db_verify_refcounts"),
        storage_access,
        scan_filter_for_shrinking,
        num_background_threads: Some(accounts_db_background_threads),
        num_foreground_threads: Some(accounts_db_foreground_threads),
        mark_obsolete_accounts,
        memlock_budget_size: solana_accounts_db::accounts_db::DEFAULT_MEMLOCK_BUDGET_SIZE,
        ..AccountsDbConfig::default()
    };

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

    let xdp_interface = matches.value_of("retransmit_xdp_interface");
    let xdp_zero_copy = matches.is_present("retransmit_xdp_zero_copy");
    let retransmit_xdp = matches.value_of("retransmit_xdp_cpu_cores").map(|cpus| {
        XdpConfig::new(
            xdp_interface,
            parse_cpu_ranges(cpus).unwrap(),
            xdp_zero_copy,
        )
    });

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

    // From now on, use run/ paths in the same way as the previous account_paths.
    let (account_run_paths, account_snapshot_paths) =
        create_all_accounts_run_and_snapshot_dirs(&account_paths)
            .map_err(|err| format!("unable to create account directories: {err}"))?;

    // These snapshot paths are only used for initial clean up, add in shrink paths if they exist.
    let account_snapshot_paths =
        if let Some(account_shrink_snapshot_paths) = account_shrink_snapshot_paths {
            account_snapshot_paths
                .into_iter()
                .chain(account_shrink_snapshot_paths)
                .collect()
        } else {
            account_snapshot_paths
        };

    let snapshot_config = new_snapshot_config(
        matches,
        &ledger_path,
        &account_paths,
        run_args.rpc_bootstrap_config.incremental_snapshot_fetch,
    )?;

    let use_snapshot_archives_at_startup = value_t_or_exit!(
        matches,
        use_snapshot_archives_at_startup::cli::NAME,
        UseSnapshotArchivesAtStartup
    );

    let mut validator_config = ValidatorConfig {
        logfile,
        require_tower: matches.is_present("require_tower"),
        tower_storage,
        halt_at_slot: value_t!(matches, "dev_halt_at_slot", Slot).ok(),
        max_genesis_archive_unpacked_size: MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
        expected_genesis_hash: matches
            .value_of("expected_genesis_hash")
            .map(|s| Hash::from_str(s).unwrap()),
        fixed_leader_schedule: None,
        expected_bank_hash: matches
            .value_of("expected_bank_hash")
            .map(|s| Hash::from_str(s).unwrap()),
        expected_shred_version,
        new_hard_forks: hardforks_of(matches, "hard_forks"),
        rpc_config: run_args.json_rpc_config,
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
        pubsub_config: run_args.pub_sub_config,
        voting_disabled: matches.is_present("no_voting") || restricted_repair_only_mode,
        wait_for_supermajority: value_t!(matches, "wait_for_supermajority", Slot).ok(),
        known_validators: run_args.known_validators,
        repair_validators,
        repair_whitelist,
        repair_handler_type: RepairHandlerType::default(),
        gossip_validators,
        max_ledger_shreds,
        blockstore_options: run_args.blockstore_options,
        run_verification: !matches.is_present("skip_startup_ledger_verification"),
        debug_keys,
        warp_slot: None,
        generator_config: None,
        contact_debug_interval,
        contact_save_interval: DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS,
        send_transaction_service_config: run_args.send_transaction_service_config,
        no_poh_speed_test: matches.is_present("no_poh_speed_test"),
        no_os_memory_stats_reporting: matches.is_present("no_os_memory_stats_reporting"),
        no_os_network_stats_reporting: matches.is_present("no_os_network_stats_reporting"),
        no_os_cpu_stats_reporting: matches.is_present("no_os_cpu_stats_reporting"),
        no_os_disk_stats_reporting: matches.is_present("no_os_disk_stats_reporting"),
        // The validator needs to open many files, check that the process has
        // permission to do so in order to fail quickly and give a direct error
        enforce_ulimit_nofile: true,
        poh_pinned_cpu_core: value_of(matches, "poh_pinned_cpu_core")
            .unwrap_or(poh_service::DEFAULT_PINNED_CPU_CORE),
        poh_hashes_per_batch: value_of(matches, "poh_hashes_per_batch")
            .unwrap_or(poh_service::DEFAULT_HASHES_PER_BATCH),
        process_ledger_before_services: matches.is_present("process_ledger_before_services"),
        account_paths: account_run_paths,
        account_snapshot_paths,
        accounts_db_config,
        accounts_db_skip_shrink: true,
        accounts_db_force_initial_clean: matches.is_present("no_skip_initial_accounts_db_clean"),
        snapshot_config,
        no_wait_for_vote_to_start_leader: matches.is_present("no_wait_for_vote_to_start_leader"),
        wait_to_vote_slot: None,
        runtime_config: RuntimeConfig {
            log_messages_bytes_limit: value_of(matches, "log_messages_bytes_limit"),
            ..RuntimeConfig::default()
        },
        staked_nodes_overrides: staked_nodes_overrides.clone(),
        use_snapshot_archives_at_startup,
        ip_echo_server_threads,
        rayon_global_threads,
        replay_forks_threads,
        replay_transactions_threads,
        tvu_shred_sigverify_threads: tvu_sigverify_threads,
        delay_leader_block_for_pending_fork: matches
            .is_present("delay_leader_block_for_pending_fork"),
        wen_restart_proto_path: value_t!(matches, "wen_restart", PathBuf).ok(),
        wen_restart_coordinator: value_t!(matches, "wen_restart_coordinator", Pubkey).ok(),
        turbine_disabled: Arc::<AtomicBool>::default(),
        retransmit_xdp,
        broadcast_stage_type: BroadcastStageType::Standard,
        block_verification_method: value_t_or_exit!(
            matches,
            "block_verification_method",
            BlockVerificationMethod
        ),
        unified_scheduler_handler_threads: value_t!(
            matches,
            "unified_scheduler_handler_threads",
            usize
        )
        .ok(),
        block_production_method: value_t_or_exit!(
            matches,
            "block_production_method",
            BlockProductionMethod
        ),
        block_production_num_workers,
        block_production_scheduler_config: SchedulerConfig {
            scheduler_pacing: value_t_or_exit!(
                matches,
                "block_production_pacing_fill_time_millis",
                SchedulerPacing
            ),
        },
        enable_block_production_forwarding: staked_nodes_overrides_path.is_some(),
        enable_scheduler_bindings: matches.is_present("enable_scheduler_bindings"),
        banking_trace_dir_byte_limit: parse_banking_trace_dir_byte_limit(matches),
        validator_exit: Arc::new(RwLock::new(Exit::default())),
        validator_exit_backpressure: [(
            SnapshotPackagerService::NAME.to_string(),
            Arc::new(AtomicBool::new(false)),
        )]
        .into(),
    };

    let reserved = validator_config
        .retransmit_xdp
        .as_ref()
        .map(|xdp| xdp.cpus.clone())
        .unwrap_or_default()
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    if !reserved.is_empty() {
        let available = core_affinity::get_core_ids()
            .unwrap_or_default()
            .into_iter()
            .map(|core_id| core_id.id)
            .collect::<HashSet<_>>();
        let available = available.difference(&reserved);
        set_cpu_affinity(available.into_iter().copied()).unwrap();
    }

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

    let maximum_local_snapshot_age = value_t_or_exit!(matches, "maximum_local_snapshot_age", u64);
    let minimal_snapshot_download_speed =
        value_t_or_exit!(matches, "minimal_snapshot_download_speed", f32);
    let maximum_snapshot_download_abort =
        value_t_or_exit!(matches, "maximum_snapshot_download_abort", u64);

    match validator_config.block_verification_method {
        BlockVerificationMethod::BlockstoreProcessor => {
            warn!(
                "The value \"blockstore-processor\" for --block-verification-method has been \
                 deprecated. The value \"blockstore-processor\" is still allowed for now, but is \
                 planned for removal in the near future. To update, either set the value \
                 \"unified-scheduler\" or remove the --block-verification-method argument"
            );
        }
        BlockVerificationMethod::UnifiedScheduler => {}
    }

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
                https://docs.anza.xyz/operations/guides/validator-start#system-tuning"
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
            validator_exit_backpressure: validator_config.validator_exit_backpressure.clone(),
            start_progress: start_progress.clone(),
            authorized_voter_keypairs: authorized_voter_keypairs.clone(),
            post_init: admin_service_post_init.clone(),
            tower_storage: validator_config.tower_storage.clone(),
            staked_nodes_overrides,
            rpc_to_plugin_manager_sender,
        },
    );

    let advertised_ip = matches
        .value_of("advertised_ip")
        .map(|advertised_ip| {
            solana_net_utils::parse_host(advertised_ip)
                .map_err(|err| format!("failed to parse --advertised-ip: {err}"))
        })
        .transpose()?;

    let advertised_ip = if let Some(cli_ip) = advertised_ip {
        cli_ip
    } else if !bind_addresses.active().is_unspecified() && !bind_addresses.active().is_loopback() {
        bind_addresses.active()
    } else if !entrypoint_addrs.is_empty() {
        let mut order: Vec<_> = (0..entrypoint_addrs.len()).collect();
        order.shuffle(&mut thread_rng());

        order
            .into_iter()
            .find_map(|i| {
                let entrypoint_addr = &entrypoint_addrs[i];
                info!(
                    "Contacting {entrypoint_addr} to determine the validator's public IP address"
                );
                solana_net_utils::get_public_ip_addr_with_binding(
                    entrypoint_addr,
                    bind_addresses.active(),
                )
                .map_or_else(
                    |err| {
                        warn!("Failed to contact cluster entrypoint {entrypoint_addr}: {err}");
                        None
                    },
                    Some,
                )
            })
            .ok_or_else(|| "unable to determine the validator's public IP address".to_string())?
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    let gossip_port = value_t!(matches, "gossip_port", u16).or_else(|_| {
        solana_net_utils::find_available_port_in_range(bind_addresses.active(), (0, 1))
            .map_err(|err| format!("unable to find an available gossip port: {err}"))
    })?;

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

    let public_tvu_addr = matches
        .value_of("public_tvu_addr")
        .map(|public_tvu_addr| {
            solana_net_utils::parse_host_port(public_tvu_addr)
                .map_err(|err| format!("failed to parse --public-tvu-address: {err}"))
        })
        .transpose()?;

    if bind_addresses.len() > 1 && public_tvu_addr.is_some() {
        Err(String::from(
            "--public-tvu-address can not be used in a multihoming context",
        ))?;
    }

    let tpu_vortexor_receiver_address =
        matches
            .value_of("tpu_vortexor_receiver_address")
            .map(|tpu_vortexor_receiver_address| {
                solana_net_utils::parse_host_port(tpu_vortexor_receiver_address).unwrap_or_else(
                    |err| {
                        eprintln!("Failed to parse --tpu-vortexor-receiver-address: {err}");
                        exit(1);
                    },
                )
            });

    info!("tpu_vortexor_receiver_address is {tpu_vortexor_receiver_address:?}");
    let num_quic_endpoints = value_t_or_exit!(matches, "num_quic_endpoints", NonZeroUsize);

    let tpu_max_connections_per_peer: Option<u64> = matches
        .value_of("tpu_max_connections_per_peer")
        .and_then(|v| v.parse().ok());
    let tpu_max_connections_per_unstaked_peer = tpu_max_connections_per_peer
        .unwrap_or_else(|| value_t_or_exit!(matches, "tpu_max_connections_per_unstaked_peer", u64));
    let tpu_max_connections_per_staked_peer = tpu_max_connections_per_peer
        .unwrap_or_else(|| value_t_or_exit!(matches, "tpu_max_connections_per_staked_peer", u64));
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
        advertised_ip,
        gossip_port,
        port_range: dynamic_port_range,
        bind_ip_addrs: bind_addresses,
        public_tpu_addr,
        public_tpu_forwards_addr,
        public_tvu_addr,
        num_tvu_receive_sockets: tvu_receive_threads,
        num_tvu_retransmit_sockets: tvu_retransmit_threads,
        num_quic_endpoints,
        vortexor_receiver_addr: tpu_vortexor_receiver_address,
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
        node.info.remove_alpenglow();

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
    snapshot_utils::remove_tmp_snapshot_archives(
        &validator_config.snapshot_config.full_snapshot_archives_dir,
    );
    snapshot_utils::remove_tmp_snapshot_archives(
        &validator_config
            .snapshot_config
            .incremental_snapshot_archives_dir,
    );

    let should_check_duplicate_instance = true;
    if !cluster_entrypoints.is_empty() {
        bootstrap::rpc_bootstrap(
            &node,
            &identity_keypair,
            &ledger_path,
            &vote_account,
            authorized_voter_keypairs.clone(),
            &cluster_entrypoints,
            &mut validator_config,
            run_args.rpc_bootstrap_config,
            do_port_check,
            use_progress_bar,
            maximum_local_snapshot_age,
            should_check_duplicate_instance,
            &start_progress,
            minimal_snapshot_download_speed,
            maximum_snapshot_download_abort,
            run_args.socket_addr_space,
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

    let tpu_quic_server_config = SwQosQuicStreamerConfig {
        quic_streamer_config: QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
            num_threads: tpu_transaction_receive_threads,
            ..Default::default()
        },
        qos_config: SwQosConfig {
            max_connections_per_unstaked_peer: tpu_max_connections_per_unstaked_peer
                .try_into()
                .unwrap(),
            max_connections_per_staked_peer: tpu_max_connections_per_staked_peer
                .try_into()
                .unwrap(),
            max_staked_connections: tpu_max_staked_connections.try_into().unwrap(),
            max_unstaked_connections: tpu_max_unstaked_connections.try_into().unwrap(),
            max_streams_per_ms,
        },
    };

    let tpu_fwd_quic_server_config = SwQosQuicStreamerConfig {
        quic_streamer_config: QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
            num_threads: tpu_transaction_forward_receive_threads,
            ..Default::default()
        },
        qos_config: SwQosConfig {
            max_connections_per_staked_peer: tpu_max_connections_per_staked_peer
                .try_into()
                .unwrap(),
            max_connections_per_unstaked_peer: tpu_max_connections_per_unstaked_peer
                .try_into()
                .unwrap(),
            max_staked_connections: tpu_max_fwd_staked_connections.try_into().unwrap(),
            max_unstaked_connections: tpu_max_fwd_unstaked_connections.try_into().unwrap(),
            max_streams_per_ms,
        },
    };

    let vote_quic_server_config = SimpleQosQuicStreamerConfig {
        quic_streamer_config: QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: tpu_max_connections_per_ipaddr_per_minute,
            num_threads: tpu_vote_transaction_receive_threads,
            ..Default::default()
        },
        qos_config: SimpleQosConfig {
            max_streams_per_second: MAX_VOTES_PER_SECOND,
            ..Default::default()
        },
    };

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
        run_args.socket_addr_space,
        ValidatorTpuConfig {
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
                error!(
                    "Please remove --wen_restart and use --wait_for_supermajority as instructed \
                     above"
                );
                exit(200);
            }
            Err(format!("{err:?}"))
        }
    }?;

    if let Some(filename) = init_complete_file {
        File::create(filename).map_err(|err| format!("unable to create {filename}: {err}"))?;
    }
    info!("Validator initialized");
    validator.listen_for_signals()?;
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
                info!("obtained shred-version {shred_version} from {entrypoint}");
                return Some(shred_version);
            }
        }
    }
    None
}

fn parse_banking_trace_dir_byte_limit(matches: &ArgMatches) -> u64 {
    if matches.is_present("disable_banking_trace") {
        // disable with an explicit flag; This effectively becomes `opt-out` by resetting to
        // DISABLED_BAKING_TRACE_DIR, while allowing us to specify a default sensible limit in clap
        // configuration for cli help.
        DISABLED_BAKING_TRACE_DIR
    } else {
        // a default value in clap configuration (BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT) or
        // explicit user-supplied override value
        value_t_or_exit!(matches, "banking_trace_dir_byte_limit", u64)
    }
}

fn new_snapshot_config(
    matches: &ArgMatches,
    ledger_path: &Path,
    account_paths: &[PathBuf],
    incremental_snapshot_fetch: bool,
) -> Result<SnapshotConfig, Box<dyn std::error::Error>> {
    let (full_snapshot_archive_interval, incremental_snapshot_archive_interval) =
        if matches.is_present("no_snapshots") {
            // snapshots are disabled
            (SnapshotInterval::Disabled, SnapshotInterval::Disabled)
        } else {
            match (
                incremental_snapshot_fetch,
                value_t_or_exit!(matches, "snapshot_interval_slots", NonZeroU64),
            ) {
                (true, incremental_snapshot_interval_slots) => {
                    // incremental snapshots are enabled
                    // use --snapshot-interval-slots for the incremental snapshot interval
                    let full_snapshot_interval_slots =
                        value_t_or_exit!(matches, "full_snapshot_interval_slots", NonZeroU64);
                    (
                        SnapshotInterval::Slots(full_snapshot_interval_slots),
                        SnapshotInterval::Slots(incremental_snapshot_interval_slots),
                    )
                }
                (false, full_snapshot_interval_slots) => {
                    // incremental snapshots are *disabled*
                    // use --snapshot-interval-slots for the *full* snapshot interval
                    // also warn if --full-snapshot-interval-slots was specified
                    if matches.occurrences_of("full_snapshot_interval_slots") > 0 {
                        warn!(
                            "Incremental snapshots are disabled, yet \
                             --full-snapshot-interval-slots was specified! Note that \
                             --full-snapshot-interval-slots is *ignored* when incremental \
                             snapshots are disabled. Use --snapshot-interval-slots instead.",
                        );
                    }
                    (
                        SnapshotInterval::Slots(full_snapshot_interval_slots),
                        SnapshotInterval::Disabled,
                    )
                }
            }
        };

    info!(
        "Snapshot configuration: full snapshot interval: {}, incremental snapshot interval: {}",
        match full_snapshot_archive_interval {
            SnapshotInterval::Disabled => "disabled".to_string(),
            SnapshotInterval::Slots(interval) => format!("{interval} slots"),
        },
        match incremental_snapshot_archive_interval {
            SnapshotInterval::Disabled => "disabled".to_string(),
            SnapshotInterval::Slots(interval) => format!("{interval} slots"),
        },
    );
    // It is unlikely that a full snapshot interval greater than an epoch is a good idea.
    // Minimally we should warn the user in case this was a mistake.
    if let SnapshotInterval::Slots(full_snapshot_interval_slots) = full_snapshot_archive_interval {
        let full_snapshot_interval_slots = full_snapshot_interval_slots.get();
        if full_snapshot_interval_slots > DEFAULT_SLOTS_PER_EPOCH {
            warn!(
                "The full snapshot interval is excessively large: {full_snapshot_interval_slots}! \
                 This will negatively impact the background cleanup tasks in accounts-db. \
                 Consider a smaller value.",
            );
        }
    }

    let snapshots_dir = matches
        .value_of("snapshots")
        .map(Path::new)
        .unwrap_or(ledger_path);
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
            "the --accounts and --snapshots paths must be unique since they both create \
             'snapshots' subdirectories, otherwise there may be collisions"
                .to_string(),
        )?;
    }

    let bank_snapshots_dir = snapshots_dir.join(BANK_SNAPSHOTS_DIR);
    fs::create_dir_all(&bank_snapshots_dir).map_err(|err| {
        format!(
            "failed to create bank snapshots directory '{}': {err}",
            bank_snapshots_dir.display(),
        )
    })?;

    let full_snapshot_archives_dir = matches
        .value_of("full_snapshot_archive_path")
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshots_dir.clone());
    fs::create_dir_all(&full_snapshot_archives_dir).map_err(|err| {
        format!(
            "failed to create full snapshot archives directory '{}': {err}",
            full_snapshot_archives_dir.display(),
        )
    })?;

    let incremental_snapshot_archives_dir = matches
        .value_of("incremental_snapshot_archive_path")
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshots_dir.clone());
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

    let maximum_full_snapshot_archives_to_retain =
        value_t_or_exit!(matches, "maximum_full_snapshots_to_retain", NonZeroUsize);
    let maximum_incremental_snapshot_archives_to_retain = value_t_or_exit!(
        matches,
        "maximum_incremental_snapshots_to_retain",
        NonZeroUsize
    );

    let snapshot_packager_niceness_adj =
        value_t_or_exit!(matches, "snapshot_packager_niceness_adj", i8);

    let snapshot_config = SnapshotConfig {
        usage: if full_snapshot_archive_interval == SnapshotInterval::Disabled {
            SnapshotUsage::LoadOnly
        } else {
            SnapshotUsage::LoadAndGenerate
        },
        full_snapshot_archive_interval,
        incremental_snapshot_archive_interval,
        bank_snapshots_dir,
        full_snapshot_archives_dir,
        incremental_snapshot_archives_dir,
        archive_format,
        snapshot_version,
        maximum_full_snapshot_archives_to_retain,
        maximum_incremental_snapshot_archives_to_retain,
        packager_thread_niceness_adj: snapshot_packager_niceness_adj,
    };

    if !is_snapshot_config_valid(&snapshot_config) {
        Err(
            "invalid snapshot configuration provided: snapshot intervals are incompatible. full \
             snapshot interval MUST be larger than incremental snapshot interval (if enabled)"
                .to_string(),
        )?;
    }

    Ok(snapshot_config)
}
