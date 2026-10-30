#[cfg(target_os = "linux")]
use {
    super::{
        capabilities::{self, XdpMode},
        xdp,
    },
    agave_xdp::transmitter::XdpConfig,
};
use {
    crate::{
        admin_rpc_service::{self, StakedNodesOverrides, load_staked_nodes_overrides},
        bootstrap,
        cli::{self},
        commands::{
            FromClapArgMatches,
            run::{
                args::{self, RunArgs},
                storage,
            },
        },
        ledger_lockfile, lock_ledger,
    },
    agave_votor::vote_history_storage::VoteHistoryStorage,
    agave_votor_transport::MAX_ENDPOINTS,
    arc_swap::ArcSwap,
    clap::{ArgMatches, crate_name, value_t, value_t_or_exit, values_t, values_t_or_exit},
    crossbeam_channel::unbounded,
    log::*,
    rand::{rng, seq::SliceRandom},
    solana_accounts_db::utils::create_and_canonicalize_directory,
    solana_clap_utils::input_parsers::{keypair_of, keypairs_of, pubkey_of, value_of},
    solana_clock::Slot,
    solana_core::{
        banking_stage::transaction_scheduler::scheduler_controller::SchedulerConfig,
        consensus::tower_storage,
        repair::repair_handler::RepairHandlerType,
        snapshot_packager_service::SnapshotPackagerService,
        system_monitor_service::SystemMonitorService,
        validator::{
            BlockProductionMethod, BlockVerificationMethod, SchedulerPacing, Validator,
            ValidatorConfig, ValidatorLogConfig, ValidatorStartProgress,
        },
    },
    solana_genesis_utils::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
    solana_gossip::{
        cluster_info::{DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS, NodeConfig},
        contact_info::ContactInfo,
        node::Node,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_options::BlockstoreCleanupStrategy,
        shred::filter::TurbineMode,
        use_snapshot_archives_at_startup::{self, UseSnapshotArchivesAtStartup},
    },
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    solana_poh::poh_service,
    solana_pubkey::Pubkey,
    solana_runtime::runtime_config::RuntimeConfig,
    solana_signer::Signer,
    solana_turbine::broadcast_stage::BroadcastStageType,
    solana_validator_exit::Exit,
    std::{
        collections::HashSet,
        env,
        fs::File,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::NonZeroUsize,
        path::PathBuf,
        str::{self, FromStr},
        sync::{Arc, RwLock, atomic::AtomicBool},
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
    config: super::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    // Debugging panics is easier with a backtrace
    if env::var_os("RUST_BACKTRACE").is_none() {
        // Safety: env update is made before any spawned threads might access the environment
        unsafe { env::set_var("RUST_BACKTRACE", "1") }
    }

    let run_args = RunArgs::from_clap_arg_match(matches)?;
    // Canonicalize ledger path to avoid issues with symlink creation
    let ledger_path = create_and_canonicalize_directory(&run_args.ledger_path).map_err(|err| {
        format!(
            "failed to create and canonicalize ledger path '{}': {err}",
            run_args.ledger_path.display(),
        )
    })?;
    let mut ledger_lock = ledger_lockfile(&ledger_path);
    let _ledger_write_guard = lock_ledger(&ledger_path, &mut ledger_lock);

    let num_threads = cli::thread_args::parse_num_threads_args(matches);
    let cli::thread_args::NumThreadConfig {
        accounts_db_background_threads: _,
        accounts_index_flush_threads: _,
        block_production_num_workers,
        ip_echo_server_threads,
        rayon_global_threads,
        replay_forks_threads,
        replay_transactions_threads,
        tpu_sigverify_threads: _,
        tpu_transaction_forward_receive_threads: _,
        tpu_transaction_receive_threads: _,
        tpu_vote_transaction_receive_threads: _,
        tvu_receive_threads,
        tvu_retransmit_threads,
        tvu_sigverify_threads,
        tvu_bls_sigverify_threads,
    } = num_threads;

    let unified_scheduler_handler_threads =
        value_t!(matches, "unified_scheduler_handler_threads", usize).ok();
    let execution_arenas = value_t!(matches, "execution_arenas", usize).ok();

    let identity_keypair = Arc::new(run_args.identity_keypair);

    let logfile = run_args.logfile;
    let log_config = if let Some(ref logfile) = logfile {
        println!("log file: {}", logfile.display());
        let logrotate_flag = Validator::register_logrotate_signal_handler()?;

        Some(ValidatorLogConfig {
            logfile: logfile.clone(),
            logrotate_flag,
        })
    } else {
        None
    };
    let use_progress_bar = log_config.is_none();
    agave_logger::initialize_logging(logfile);

    cli::warn_for_deprecated_arguments(matches);

    info!("{} {}", crate_name!(), solana_version);
    info!("Starting validator with: {:#?}", std::env::args_os());

    solana_metrics::set_host_id(identity_keypair.pubkey().to_string());
    solana_metrics::set_panic_hook("validator", Some(String::from(solana_version)));

    let bind_addresses = {
        let parsed = matches
            .values_of("bind_address")
            .expect("bind_address should always be present due to default")
            .map(solana_net_utils::parse_host)
            .collect::<Result<Vec<_>, _>>()?;
        BindIpAddrs::new(parsed).map_err(|err| format!("invalid bind_addresses: {err}"))?
    };

    let entrypoint_addrs = run_args.entrypoints;
    for addr in &entrypoint_addrs {
        if !run_args.socket_addr_space.check(addr) {
            Err(format!("invalid entrypoint address: {addr}"))?;
        }
    }
    #[cfg(target_os = "linux")]
    let poh_pinned_cpu_core = value_of(matches, "poh_pinned_cpu_core")
        .or_else(|| value_of(matches, "experimental_poh_pinned_cpu_core"))
        .or(poh_service::DEFAULT_PINNED_CPU_CORE);

    #[cfg(not(target_os = "linux"))]
    let poh_pinned_cpu_core = None;

    // XDP is not needed for init — it only initializes the ledger and exits.
    // Also, init drops all Linux capabilities in main() so XDP setup would fail.
    #[cfg(target_os = "linux")]
    let xdp_transmit_config: Option<XdpConfig> = if operation == Operation::Initialize {
        None
    } else {
        args::xdp_config::new_config(matches, &bind_addresses, poh_pinned_cpu_core)?
    };

    let dynamic_port_range =
        solana_net_utils::parse_port_range(matches.value_of("dynamic_port_range").unwrap())
            .expect("invalid dynamic_port_range");

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
        order.shuffle(&mut rng());

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

    let num_quic_endpoints = value_t_or_exit!(matches, "num_quic_endpoints", NonZeroUsize);
    let num_votor_quic_endpoints = value_t_or_exit!(matches, "num_votor_endpoints", NonZeroUsize);
    if num_votor_quic_endpoints.get() > MAX_ENDPOINTS {
        Err(format!(
            "--num-votor-endpoints must be at most {MAX_ENDPOINTS}"
        ))?;
    }

    let node_config = NodeConfig {
        advertised_ip,
        gossip_port,
        port_range: dynamic_port_range,
        bind_ip_addrs: bind_addresses.clone(),
        public_tpu_addr,
        public_tpu_forwards_addr,
        public_tvu_addr,
        num_tvu_receive_sockets: tvu_receive_threads,
        num_tvu_retransmit_sockets: tvu_retransmit_threads,
        num_quic_endpoints,
        num_votor_quic_endpoints,
    };

    let mut node = Node::new_with_external_ip(&identity_keypair.pubkey(), node_config);

    let exit = Arc::new(AtomicBool::new(false));

    #[cfg(not(target_os = "linux"))]
    let _ = config;

    #[cfg(target_os = "linux")]
    let (xdp_transmit_setup, xdp_network_config_report) = {
        let needs_sys_nice = value_t_or_exit!(matches, "snapshot_packager_niceness_adj", i8) != 0
            || run_args.json_rpc_config.rpc_niceness_adj != 0;
        let xdp_mode = match xdp_transmit_config.as_ref() {
            None => XdpMode::Disabled,
            Some(xdp_config) if xdp_config.zero_copy => XdpMode::ZeroCopy,
            Some(_) => XdpMode::Copy,
        };
        capabilities::restrict(config.primordial_caps, needs_sys_nice, xdp_mode, || {
            xdp_transmit_config.map(|xdp_config| {
                xdp::build_xdp_transmit_setup(xdp_config, &node.bind_ip_addrs, exit.clone())
            })
        })?
        .transpose()?
        .unzip()
    };

    #[cfg(not(target_os = "linux"))]
    let (xdp_transmit_setup, xdp_network_config_report) = (None, None);

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

    let blockstore_cleanup_strategy = BlockstoreCleanupStrategy::from_clap_arg_match(matches)?;

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
    let votor_peer_overrides = validators_set(
        &identity_keypair.pubkey(),
        matches,
        "votor_peer_overrides",
        "--votor-peer-overrides",
    )?;
    // Identities named on the command line carry no address: the peer list resolves
    // them from gossip.
    let votor_peer_overrides = Arc::new(ArcSwap::from_pointee(
        votor_peer_overrides
            .unwrap_or_default()
            .into_iter()
            .map(|identity| (identity, None))
            .collect(),
    ));

    if bind_addresses.len() > 1 {
        for (flag, msg) in [
            (
                "advertised_ip",
                "--advertised-ip cannot be used in a multihoming context. In multihoming, the \
                 validator will advertise the first --bind-address as this node's public IP \
                 address.",
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

    let restricted_repair_only_mode = matches.is_present("restricted_repair_only_mode");
    let use_direct_io = !matches.is_present("no_accounts_db_snapshots_direct_io");
    let accounts_db_config =
        args::accounts_db_config::new_config(matches, &ledger_path, &num_threads, use_direct_io)?;
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

    let snapshot_config = args::snapshot_config::new_config(
        matches,
        &ledger_path,
        args::snapshot_config::SnapshotOptions {
            incremental_snapshot_fetch: run_args.rpc_bootstrap_config.incremental_snapshot_fetch,
            use_direct_io,
        },
    )?;

    let use_snapshot_archives_at_startup = value_t_or_exit!(
        matches,
        use_snapshot_archives_at_startup::cli::NAME,
        UseSnapshotArchivesAtStartup
    );

    let skip_transaction_signatures_in_status_cache =
        !run_args.json_rpc_config.full_api && !snapshot_config.should_generate_snapshots();
    if skip_transaction_signatures_in_status_cache {
        info!(
            "Transaction signatures will not be stored in the status cache because full RPC and \
             snapshot generation are disabled"
        );
    }

    let vote_history_storage = Some(Arc::new(VoteHistoryStorage::new(ledger_path.clone())));

    let mut validator_config = ValidatorConfig {
        log_config,
        require_tower: false,
        require_vote_history: !matches.is_present("do_not_require_vote_history"),
        tower_storage,
        vote_history_storage,
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
        runtime_config: RuntimeConfig {
            log_messages_bytes_limit: value_of(matches, "log_messages_bytes_limit"),
            skip_transaction_signatures_in_status_cache,
            ..RuntimeConfig::default()
        },
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
        should_check_duplicate_instance: true,
        repair_whitelist,
        votor_peer_overrides,
        repair_handler_type: RepairHandlerType::default(),
        gossip_validators,
        blockstore_cleanup_strategy,
        blockstore_options: run_args.blockstore_options,
        run_verification: !matches.is_present("skip_startup_ledger_verification"),
        debug_keys,
        filter_keys: Arc::new(run_args.filter_keys),
        warp_slot: None,
        generator_config: None,
        contact_debug_interval,
        contact_save_interval: DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS,
        send_transaction_service_config: run_args.send_transaction_service_config,
        no_poh_speed_test: matches.is_present("no_poh_speed_test"),
        no_os_memory_stats_reporting: matches.is_present("no_os_memory_stats_reporting"),
        no_os_network_stats_reporting: matches.is_present("no_os_network_stats_reporting"),
        xdp_network_config_report,
        no_os_cpu_stats_reporting: matches.is_present("no_os_cpu_stats_reporting"),
        no_os_disk_stats_reporting: matches.is_present("no_os_disk_stats_reporting"),
        // The validator needs to open many files, check that the process has
        // permission to do so in order to fail quickly and give a direct error
        enforce_ulimit_nofile: true,
        poh_pinned_cpu_core,
        poh_hashes_per_batch: value_of(matches, "poh_hashes_per_batch")
            .unwrap_or(poh_service::DEFAULT_HASHES_PER_BATCH),
        process_ledger_before_services: matches.is_present("process_ledger_before_services"),
        account_paths,
        accounts_db_config,
        snapshot_config,
        no_wait_for_vote_to_start_leader: matches.is_present("no_wait_for_vote_to_start_leader"),
        wait_to_vote_slot: value_t!(matches, "wait_to_vote_slot", Slot).ok(),
        staked_nodes_overrides: staked_nodes_overrides.clone(),
        use_snapshot_archives_at_startup,
        ip_echo_server_threads,
        rayon_global_threads,
        replay_forks_threads,
        replay_transactions_threads,
        tvu_shred_sigverify_threads: tvu_sigverify_threads,
        tvu_bls_sigverify_threads,
        delay_leader_block_for_pending_fork: !matches
            .is_present("no_delay_leader_block_for_pending_fork"),
        turbine_mode: TurbineMode::default(),
        broadcast_stage_type: BroadcastStageType::Standard,
        block_verification_method: value_t_or_exit!(
            matches,
            "block_verification_method",
            BlockVerificationMethod
        ),
        unified_scheduler_handler_threads,
        execution_arenas,
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
        validator_exit: Arc::new(RwLock::new(Exit::default())),
        validator_exit_backpressure: [(
            SnapshotPackagerService::NAME.to_string(),
            Arc::new(AtomicBool::new(false)),
        )]
        .into(),
        snapshot_packager_niceness_adj: value_t_or_exit!(
            matches,
            "snapshot_packager_niceness_adj",
            i8
        ),
    };

    storage::prepare(
        &mut validator_config.account_paths,
        &mut validator_config.snapshot_config,
    )?;

    validator_config
        .block_production_method
        .warn_if_deprecated_value();

    let vote_account = pubkey_of(matches, "vote_account").unwrap_or_else(|| {
        if !validator_config.voting_disabled {
            warn!("--vote-account not specified, validator will not vote");
            validator_config.voting_disabled = true;
        }
        Keypair::new().pubkey()
    });

    let maximum_local_snapshot_age = value_t_or_exit!(matches, "maximum_local_snapshot_age", u64);
    let minimal_snapshot_download_speed =
        value_t_or_exit!(matches, "minimal_snapshot_download_speed", f32);
    let maximum_snapshot_download_abort =
        value_t_or_exit!(matches, "maximum_snapshot_download_abort", u64);

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
            vote_history_storage: validator_config.vote_history_storage.clone(),
            staked_nodes_overrides,
            rpc_to_plugin_manager_sender,
        },
    );

    let tpu_config = args::tpu_config::new_config(matches, &num_threads)?;

    let cluster_entrypoints = entrypoint_addrs
        .iter()
        .map(ContactInfo::new_gossip_entry_point)
        .collect::<Vec<_>>();

    if restricted_repair_only_mode {
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

    let validator = Validator::new_with_exit(
        node,
        identity_keypair,
        &ledger_path,
        &vote_account,
        authorized_voter_keypairs,
        cluster_entrypoints,
        &validator_config,
        rpc_to_plugin_manager_receiver,
        start_progress,
        run_args.socket_addr_space,
        tpu_config,
        admin_service_post_init,
        xdp_transmit_setup,
        exit,
    )
    .map_err(|err| format!("{err:?}"))?;

    if let Some(filename) = init_complete_file {
        File::create(filename).map_err(|err| format!("unable to create {filename}: {err}"))?;
    }
    info!("Validator initialized");
    validator.listen_for_signals()?;
    validator.close();
    info!("Validator exiting...");

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
        index.shuffle(&mut rand::rng());
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
